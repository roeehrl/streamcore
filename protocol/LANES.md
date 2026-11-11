# Secure transport lanes (major 2)

Native sessions negotiate the session ALPN (`SESSION_ALPN` in `identity.rs`; its
value still carries the original product name, see the README) and Hello major 2
(`protocol::PROTOCOL_MAJOR`). There is no major-1 fallback. Binary payloads use
`frames.v3` (`media::FRAME_CAPABILITY`, see [BINARY.md](BINARY.md)). StreamCore does
not define an application control protocol: each product carries and versions its
own control messages, and places them on lanes with its `multiplex::LanePolicy`.
This document describes the implementation, not a qualified release or a measured
latency guarantee.

## Ordering and admission

Every complete application frame is classified by Rust before admission. Each lane
is FIFO in the order of synchronous admission. Messages in different lanes may
overtake one another. A caller must not assume global wire order.

| ID | Lane | Default traffic (`binary_lanes`) | Receive byte window |
| --- | --- | --- | --- |
| 0 | Interactive | Every frame kind without another default: input (`0x20`), ping and pong (under `clock.v1` sent and answered by the core, [BINARY.md](BINARY.md#typed-ping-and-pong-clockv1)) and product control | 4 MiB |
| 1 | Video | Video frames (`0x10`) | 32 MiB + 5 bytes |
| 2 | Audio | Audio packets (`0x11`) | 2 MiB |
| 3 | Terminal | Terminal data (`0x30`) | 8 MiB |
| 4 | Bulk | File data (`0x40`) | 16 MiB |

A product's lane policy may move one of its control messages onto the lane of the
data it describes (for example a stream start or codec config with its video, or a
file offer, accept and completion with its chunks), so that setup and data stay in
order on one lane.

The five-byte application envelope counts against each window. A single message
cannot exceed its lane's window. Each lane permits at most 64 outstanding messages;
the receiver counts incomplete, queued and consumed-but-not-yet-credited messages
together. Thus admitted receive payload storage is bounded by 62 MiB + 5 bytes.
Transport, codec and native application buffers are additional allocations.

The sender admits at most 128 messages / 64 MiB across all lanes. Noninteractive
admission stops at 112 messages / 60 MiB, reserving capacity for interactive work.
Excess admission returns Backpressure. These are hard safety bounds; they do not
replace capture throttling or a native consumer's own queue limits.

Unit tests check the default classifier for every binary kind and for an
overriding policy; a product tests its own policy against its control fixtures.
An explicit raw-lane enqueue exists for low-level diagnostics; native application
sends use the classifier. Semantic command acknowledgements belong to the
product's control protocol, and sequenced terminal state to the binary contract.
Because different lanes may overtake one another, a product must not infer
readiness from cross-lane arrival order (for example, terminal attachment should
wait for both its acknowledgement and its snapshot).

## QUIC and TLS/TCP

After mutual TLS and Hello negotiation, QUIC uses its first bidirectional stream
for lane 0. The client opens four more streams in lane order, sends the lane ID
byte, and waits for the server to echo it. IDs and order are checked under the
existing five-second handshake deadline. The connection permits exactly five
bidirectional streams and no unidirectional streams.

QUIC stream priorities are interactive 2, audio/terminal 1, video/bulk 0. Each
stream has its own driver and credit admission. Loss on one stream does not impose
stream ordering on another, though congestion control and connection-wide flow
control remain shared. See [RFC 9000 section 2](https://www.rfc-editor.org/rfc/rfc9000.html#section-2)
and [Quinn SendStream priority](https://docs.rs/quinn/0.11.12/quinn/struct.SendStream.html#method.set_priority).

TLS/TCP carries all lanes on one ordered socket. The scheduler advances through
`0,2,0,3,0,1,0,4,0,2,0,3,0,1,0,1`, skips ineligible lanes, and writes at most 16 KiB
of message data per turn. An already-writing fragment cannot be preempted. This
provides bulk progress and frequent interactive opportunities, but cannot remove
TCP's transport-level head-of-line blocking. Complete-message delivery uses the
same weighted order on both transports.

## Record format

Every record starts with its u32 big-endian byte length, excluding that length
field. All integers below are big endian. Maximum record length is 16,402 bytes.

| Kind | Fields after record length |
| --- | --- |
| Data | kind=0 u8, lane u8, sequence u64, total message bytes u32, offset u32, 1–16,384 data bytes |
| Credit | kind=1 u8, lane u8, sequence u64, consumed message bytes u32 |

Message sequences start at 1 independently in each direction/lane, increment
exactly, and cannot wrap. A first fragment has offset 0; later fragments must
match the sequence, total and exact next offset. One incomplete message is allowed
per lane. Wrong physical QUIC lane, unknown tags, invalid sizes, sequence gaps,
offset overflow, duplicate credits or a credit with the wrong byte count close
the channel. Receiver capacity is checked before allocating a message buffer.

Credits travel on QUIC lane 0 or the shared TCP socket. They are granted only when
the native consumer takes a complete message, not merely when a socket receives
it. Receive accounting is released when that credit is scheduled for transmission.
Credits are served in bursts of at most eight before checking local data, preventing
continuous peer consumption from starving local interactive sends. All pending
credits remain bounded by the per-lane message/byte windows.

## Completion and close

`PendingSend.written()` means the bytes were handed to the transport.
`PendingSend.consumed()` means the remote channel's consumer took the complete
message. Neither means a command executed, input was injected, a file was committed
or a human saw a result. Each waiter can be awaited once.

The host's final refusal/goodbye waits for consumption before closing, with a
two-second hard teardown bound. Closing after a local QUIC write alone can discard
the final message. Ordinary sends await written completion. Swift callbacks fire
exactly once, including close and deinitialization; failed non-disposable admission
closes the native channel. Disposable media may be dropped under pressure without
closing; its false callback reports that drop. There is no automatic replay on reconnect.

Explicit local close discards queued reads, fails pending sends and releases
transport handles. Peer/transport closure preserves complete messages already
received, then delivers EOF; partial assemblies are discarded. Cancelling the
native reader closes the connection rather than reusing interrupted framing.

## Evidence and remaining work

Unit tests check fragment scheduling, reserved interactive capacity, receive/credit
bounds, malformed records, duplicate credit rejection and classification of all
control fixtures. QUIC/TCP socket tests fill the bulk window, demonstrate a
pending bulk send, deliver interactive key-up, then consume bulk and observe it
resume. Native Swift tests verify offer/chunk/done ordering and final-message
consumption before close on both transports, including iOS Simulator execution.

Native [capture admission](../docs/CAPTURE-PIPELINE.md) bounds encoder/output work
and keeps one latest waiting raw frame. The [scheduling probe](../ci/scheduling/README.md)
measures this channel under a declared five-lane workload (120-Hz video-sized
messages with concurrent interactive, audio, terminal and bulk load) between two
machines. This repository records no measurement results; products that run the
probe keep their results and environment evidence in their own records. Synthetic
transport measurements do not establish smooth rendering at any frame rate.
Display-pipeline qualification, real-network latency and long soak runs belong to
the products that integrate StreamCore.

## Stage timing

For per-stage measurement (docs/STREAMCORE-VIDEO.md 9.2), `SecureChannel::receive_timed`
returns a message with the times its first and last fragments arrived, and
`PendingSend::written_at_us` the time its last fragment was handed to the transport. Both
use `clock::monotonic_now_us`; neither changes scheduling.
