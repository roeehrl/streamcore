# Binary media contract

Native authenticated sessions require the `frames.v3` capability
(`media::FRAME_CAPABILITY`) in Hello major 2. A peer that only supports an earlier
frames version is rejected during capability negotiation. Stream setup (which
codec, dimensions and stream ID a session uses) is carried by each product's own
control protocol, which StreamCore does not version; that protocol should name the
codec the host's encoder actually selected rather than the one requested.

The outer frame remains `[type:u8][length:u32 big endian][payload]`, with a 32 MiB
payload limit. Video is type `0x10`. All video header integers below are unsigned
and **little endian**. The complete video payload must be 30 bytes or longer
(a 29-byte header followed by nonempty codec/sample data), within the outer cap.

| Offset | Width | Value |
| --- | --- | --- |
| 0 | 4 | Session-local stream ID |
| 4 | 1 | Flags: 0 delta, 1 keyframe, 2 codec configuration |
| 5 | 8 | Nonzero encoder/format generation |
| 13 | 8 | Sample submission sequence, nonzero for samples; zero for config |
| 21 | 8 | Presentation timestamp in microseconds; zero for config |
| 29 | remaining | AVCC/HVCC sample or avcC/hvcC configuration atom |

Unknown flags, combined config/keyframe flags, zero generation, empty data,
zero sample sequence and nonzero config sequence/timestamp are invalid. Swift
`VideoPacket` and Rust `media::VideoPacket` validate the same layout. Rust parsing
borrows the payload; it does not duplicate the NALUs. MyCNC's `test-video-contract.py`
checks explicit byte/field fixtures and exchanges actual encoder output between
both implementations, including 64-bit boundary values and truncated headers.

## Continuity and ownership

`VideoPacketizer` and `VideoReceiver` are shared Rust state machines exposed via
UniFFI. Only headers cross that boundary; ScreenCaptureKit/VideoToolbox and native
sample buffers remain on the native side. Generations increase across packetizers
within the host process. A restarted native encoder or changed format gets a new
generation. A new authenticated session has independent receiver state; these
numbers are not identities or resume tokens.

The host attaches the format atom to each encoder callback and records submission
order before encoding. The host stream serializes output, discards callbacks from
superseded encoders and ignores old SCStream callbacks. The packetizer rejects
late/duplicate submission callbacks. Sequence numbers therefore retain gaps caused
by encoder omission or transport congestion shedding, rather than renumbering the
surviving samples into a false contiguous sequence.

A codec config accompanies the first sample and every keyframe. Replaying a config
within the same generation does not reset a healthy receiver. A newer generation
invalidates old references; an older generation cannot roll back the decoder. A
missing delta invalidates the reference chain until a new keyframe arrives. A
keyframe may skip sequences. Old/duplicate samples cannot repair a newer chain.
A failed config application permits the repeated config to be retried. Layer
attachment, background flush and decode failure require a fresh keyframe. Client
recovery requests remain limited to one per 500 ms.

## Audio, terminal and file payloads

Every payload is bounded by the outer 32 MiB limit. Data regions must be
nonempty except an empty external-terminal transcript snapshot; transfer
completion is the control `fileDone` message, not an empty chunk.
Integer payload fields are little endian. UUIDs use their 16 RFC/network-order
bytes, matching Swift UUID's `uuid` tuple and Rust `[u8; 16]`; UUID version bits are
opaque to these codecs. Names and UUIDs alone confer no authority: resource lookup
and authentication belong to the session layer.

| Frame tag | Payload, in order | Header bytes |
| --- | --- | --- |
| `0x11` audio | PTS u64, AAC bytes | 8 |
| `0x30` terminal | Agent UUID, subscription UUID, kind u8, sequence u64, columns u16, rows u16, state/output bytes | 45 |
| `0x40` file | Transfer UUID, offset u64, file bytes | 24 |

A file chunk whose `offset + data length` exceeds u64 is invalid. The file-transfer
owner additionally checks the accepted offer's bounds and resource scope. The
binary parser does not claim that an opaque audio packet, terminal escape sequence
or file content is semantically valid for its downstream consumer.

Terminal kind 0 is a complete host-model replay snapshot; kind 1 is subsequent
PTY output. Both require 2–4096 columns, 1–4096 rows and at most 262144 cells.
Kind 2 replaces an external terminal's bounded text transcript; both dimensions
must be zero because the host does not own or know that terminal's grid. Other
kinds are invalid. Terminal data is capped at 4 MiB. This layout requires
`frames.v3`; there is no legacy terminal decoder.

A PTY sequence increments for every model output or resize. Subscription captures
the snapshot and its watermark together. Already-queued output at or below the
watermark is excluded for that subscriber. The client installs a snapshot before
input is enabled, even if the independent control acknowledgement arrived first.
Deltas require the next sequence and the installed dimensions. A resize sends a
fresh snapshot at the effective minimum geometry across viewers. Snapshot install
discards old parser continuation; renderer query replies are suppressed because
the host model answers the PTY. External transcripts are explicitly labeled and
replaced as snapshots, never described as complete terminal state.

## Input payloads (`0x20`)

The first byte selects exactly one fixed-size event. Integers are unsigned little
endian. Floats use IEEE-754 binary32 bits, also little endian. Every input float
must be finite; normalized x, y and pressure must be in `[0, 1]`. Trailing bytes,
unknown enum values, noncanonical booleans and unknown modifier bits are rejected.
Invalid input cannot reach native injection through the production Swift decoder.

| Tag | Fields after tag, in order | Total bytes |
| --- | --- | --- |
| 1 pointer | stream u32, phase u8, button u8, x f32, y f32, clicks u8, pressure f32, modifiers u32 | 24 |
| 2 scroll | stream u32, x f32, y f32, deltaX f32, deltaY f32, precise bool, momentum u8, modifiers u32 | 27 |
| 3 magnify | stream u32, x f32, y f32, magnification f32, phase u8 | 18 |
| 4 key | stream u32, keyCode u16, down bool, modifiers u32 | 12 |

- Booleans are 0 or 1.
- Pointer phase: down 0, moved 1, up 2, hover 3; button: left 0, right 1, middle 2.
- Magnify phase: began 0, changed 1, ended/cancelled 2.
- Momentum: none 0, begin 1, continue 2, end 3. macOS translates NSEvent's phase
  flags into these CGMomentumScrollPhase values, including continuation and end.
- Modifier bits 0 through 5 represent capsLock, shift, control, option, command and
  function. Other bits are invalid. Click counts and key codes use the full stated
  unsigned range; native adapters enforce their own platform mapping.

## Typed ping and pong (`clock.v1`)

With the `clock.v1` capability (`clock::CLOCK_CAPABILITY`), ping `0x02` and pong
`0x03` carry fixed little-endian bodies, and the StreamCore core sends and answers
them on lane 0; applications never send or answer either kind. It is a hard break:
consumers require `clock.v1`, and a peer without it fails negotiation. Without it,
both kinds stay ordinary application frames (the untyped echo with any body).

| Kind | Payload, in order | Bytes |
| --- | --- | --- |
| `0x02` ping | nonce u64, sender's send time u64 µs | 16 |
| `0x03` pong | the ping's nonce u64, answerer's receive time u64 µs, answerer's send time u64 µs | 24 |

Times are microseconds of the sending process's own monotonic clock, which has an
arbitrary origin (`clock::monotonic_now_us`). The ping's send time and the pong's
send time are written as the message's first fragment is handed to the writer; the
pong's receive time is taken when the ping's last fragment arrives. A pong whose send
time is earlier than its receive time, any other body length, or a typed frame of
either kind from an application is invalid; an invalid body closes the channel. A
pong whose nonce matches no recent ping is ignored. Both kinds are still delivered
to the application after the core has handled them, as liveness signals.

From the four times `t1` (ping sent), `t2`, `t3` (pong times) and `t4` (pong
received), one sample gives round trip `(t4 − t1) − (t3 − t2)` and offset
`((t2 − t1) + (t3 − t4)) / 2` (peer clock minus local clock), true within ± half
that round trip. `SecureChannel::clock_estimate` reports the minimum-round-trip
sample of the last 10 s, its bound, and the drift fitted over the low-delay samples
of up to 256 kept. One-way figures derived from it carry the same bound. Fixtures:
`crates/streamcore-media/tests/binary_contract.rs`.

## Envelope and verification

The outer type byte accepts control `0x01`, ping `0x02`, pong `0x03`, video `0x10`,
audio `0x11`, input `0x20`, terminal `0x30` and file `0x40`. Its length is the sole
big-endian integer in this contract. The envelope accepts empty payloads (an untyped
keepalive without `clock.v1` uses them); each typed codec imposes its own body
requirements. Unknown types and
oversized lengths fail before waiting for the body. Swift's frame encoder rejects
oversized outbound payloads before narrowing the length to u32. Rust's borrowed
frame reader returns consumed length so coalesced frames remain distinct; Swift's
Deframer accepts fragmented/coalesced byte streams.

MyCNC's `test-binary-contract.py` runs 420 explicit valid/malformed fixtures through
the production Swift and Rust codecs, checks the decoded fields against the stated
layout, and sends each implementation's actual encoded output to the other. These
include every enum value, all modifier bits, invalid booleans, truncation/trailing
bytes, NaN/infinity/out-of-range coordinates, UUID byte order, file overflow and
outer frame types. Rust and Swift unit tests also exercise the exact 32 MiB limit,
first rejected size, payload borrowing and fragmented/coalesced reads.

## Remaining work

The transport now carries these messages on five bounded [ordered lanes](LANES.md),
with QUIC stream priorities and weighted TLS fragment scheduling. Video continuity
makes congestion drops detectable. Native [capture admission](../docs/CAPTURE-PIPELINE.md)
bounds encoding and deferred output. Full display-pipeline qualification,
session resumption and a complete StreamCore remain open. Terminal replay and richer resource
scope/freshness belong to the later session/Phase 2 work. No capture/render latency
or 120-fps performance claim follows from the synthetic eight-stream continuity
test. Those measurements and the full real-device/VM soak remain Phase 1 gates.
