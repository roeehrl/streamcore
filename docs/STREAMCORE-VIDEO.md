# StreamCore video path and USB transport (design)

Design draft, revised after review. Nothing in it is implemented yet.

**Baseline.** Paths refer to this repository's `main` after `v0.1.0`: the crates are unchanged
since that tag and only documents changed. P0's citation re-check records the tag that fixes them.
The reference implementation (MyCNC) is described by behaviour only; its code citations stay in
that consumer's repository.

**Context.** StreamCore has two consumers. MyCNC links the StreamCore crates into its core; its
macOS host and Apple clients carry today's video path, which this design ports. Pad moves onto
StreamCore later and adopts the same control hello then. Each consumer keeps its own migration
plan, devices and field evidence in its own repository. The **host app** captures and encodes, the
**client app** decodes and presents, and a **consumer** is a product built on StreamCore.

**Names.** The legacy wire constants (ALPN `mycnc/2`, `peer.mycnc.invalid`, `mycnc-pairing-*`;
[README](../README.md#legacy-identifiers)) do not change. The runtime thread is `streamcore-network`
(`crates/streamcore-transport/src/runtime.rs:55`). **Transport Hello** is `protocol::Hello`
(`{major, capabilities, required}`, major 2, `crates/streamcore-transport/src/protocol.rs:27-31`),
exchanged inside `dial` and accept. **Control hello** is a consumer's first application message and
its ack, which carries the resume reference. **Link hello** is the AOA link layer's first frame
(8.4). "Hello" never appears alone.

Every millisecond figure not marked "measured" is an estimate; none closes a gate. VM results stay
labelled VM.

## Decisions

| # | Decision | Section |
| --- | --- | --- |
| D1 | Rust runs the control plane of each stream: lifecycle, sizing, validation, admission, latest-frame retention, recovery, pacing, ABR, canvas policy, packetizing, send policy and telemetry. Native code runs the data plane on GPU surfaces: capture, convert/compose, encode, decode and present. The control plane is a new crate, `crates/streamcore-video`. | 3 |
| D2 | Pixels never cross into Rust. Rust holds opaque surface leases and metadata. From P2, encoded bytes cross once, through a dedicated send call, never inside trait records. In P1 only, they make one extra trip through the `VideoOut` hop to the consumer's existing send path (3.5). | 3.2, 3.5 |
| D3 | One backend trait set (`CaptureBackend`, `Converter`, `Compositor`, `EncoderFactory`/`Encoder`, client `Decoder`). Apple and Android implement them as UniFFI foreign traits; Windows and Linux as plain Rust traits in platform crates. | 3.4, 3.8 |
| D4 | Each stream has exactly one serial executor. Native callbacks post events and return; Rust calls native code only from that executor. On macOS the executor is the stream's capture queue, which keeps the single-serial-queue confinement the reference host's fences rely on. A written reentrancy rule is normative. | 3.5 |
| D5 | Windows host (phase P5): Windows.Graphics.Capture into D3D11, one GPU convert pass, the Media Foundation hardware MFT as the universal encoder, NVENC as the first vendor tier, explicit adapter choice, the Microsoft software MFT as CPU fallback. No ffmpeg pipe, no x264. Details are settled by the P5 spike. | 4.2 |
| D6 | Linux host later (P7): portal ScreenCast + PipeWire DMA-BUF into VAAPI or NVENC. | 4.3 |
| D7 | Drop before encode; after the encoder, shed only discardable frames by default. | 6 |
| D8 | Measure first: per-stage timestamps keyed by `(stream, generation, sequence)`, a clock offset from a timestamped Rust ping, and a frame-counter pacing test. No pacing change ships without stage attribution (R3). | 9 |
| D9 | Video stays on reliable lane 1 in v1; QUIC datagrams only after gate G-DG. | 7 |
| D10 | USB is a third transport: TLS over a byte stream on the TLS/TCP framing path, with the same pinned mutual TLS, ALPN, transport Hello, certificate pins and pairing as the network. No plaintext mode. The TLS role follows the product role (host = server). The host opens every USB pipe. A pipe is a single-use dial target and is never retried. | 8 |
| D11 | USB ↔ network switches are make-before-break under one client transport owner, with a compare-and-swap resume; the stream keeps its ID. | 8.7–8.9 |
| D12 | Rollout order: instrument macOS, port the core on macOS, USB on macOS hosts, then the Android client, the Windows host, the Linux host and datagrams. | 11 |
| D13 | Every contract bump is a hard capability break. Host and client ship together, and each bump can be rolled back by shipping the previous pair together. | 10, 11.1 |
| D14 | StreamCore is its own repository, AGPL-3.0; outside contributions will need a CLA (not yet published). Consumers pin its tags as git submodules. GitHub self-hosted runners are never registered on this repository. CI runs on the local runner (`ci/local`) while the repository is private; after it is public, GitHub-hosted runners run the Rust and contract jobs only. VM, device, GPU and USB qualification runs outside GitHub Actions. | 9.7 |
| D15 | No interim latency fixes in a consumer's existing video path; moving onto StreamCore is the fix, and StreamCore must meet R1–R4. | 1.1 |
| D16 | Physical device, GPU and USB qualification is part of each phase gate. | 9.7 |

## 1. Goals and requirements

Goals: one low-latency video path for every consumer (hosts macOS, Windows, later Linux; clients
iPadOS/iOS, macOS, Android, later desktop); capture, convert/compose and encode GPU-resident on
every host with a declared CPU fallback; USB as a first-class medium with the same lanes, pinned
mutual TLS and session resume as the network, hot-plugged in both directions; every stage measured
on one clock model.

Non-goals: any plaintext or token transport; HDR10 and AV1 in v1 (reserved in the interfaces); a
Windows secure-desktop helper in the first Windows release; slice-level pipelining.

### 1.1 Latency requirements R1–R4 (project decision)

Every StreamCore host and client must meet these:

- **R1 Host: send on completion.** Each encoded frame goes to the transport as soon as the encoder
  reports it complete; frame boundaries come from the encoder API, never from the next start code.
- **R2 Client: present on decode.** Each decoded frame is presented as soon as it is decoded
  (Responsive mode: at the next vsync), never held until the next input or frame arrives.
- **R3 Per-stage timestamps** from source present (H0) to client present (C5), on one clock model
  (9.2, 9.5).
- **R4 Latest frame wins; hard bounds where it cannot.** A newer frame replaces an older one wherever
  that does not break the decode chain: raw frames (the latest slot), discardable encoded frames and
  decoded frames waiting for present. A reference delta gets a hard bound instead of a FIFO:
  admission keeps at most one unsent frame per stream (6.2), the client queues at most two for the
  decoder, and a delta is dropped by age only where LTR or temporal layers make recovery cheap (6.4).

Each hold that R1 or R2 forbids adds up to one frame interval, more as the frame rate drops. R1 and
R2 are pass/fail checks (9.6).

### 1.2 Proposed targets (open, Q8)

This document's proposal: glass-to-glass below 50 ms at p95 on a wired or USB link with a 120 Hz
reference client (Wi-Fi and 60 Hz reported, not gated); ≤ 1% skipped and ≤ 1% repeated frames for a
60 fps source on an unloaded physical host. Stretch: below 25 ms at p50 on a 120 Hz wired or USB
client.

## 2. Baseline

### 2.1 Contract versions

Transport Hello major 2, ALPN `mycnc/2` (`crates/streamcore-transport/src/protocol.rs:8`,
`identity.rs:14-15`); media `frames.v3`, a 29-byte LE video header with a 32 MiB cap
(`crates/streamcore-media/src/media.rs:3-5`); each consumer's control protocol version is a required
exact transport Hello capability that the consumer bumps. This design moves frames.v3 → frames.v4
and adds lane-0 video kinds (9.4).

### 2.2 What StreamCore has today

- `crates/streamcore-media/src/stream.rs`: `VideoWorkQueue` (at most 3 permits; idempotent release,
  also on drop, taking the permit's lock and then the queue's; `:21-83`), `valid_video_settings`
  (2–8192 px, 1–120 fps, quality 0–1; `:14-19`), `VideoPacketizer` / `VideoReceiver` (`:97-238`;
  encoder output order, late sequences dropped, `:111-122`; after a gap in the same generation a
  keyframe decodes and a delta returns `Recover`, `:212-225`). All are `Mutex`-guarded objects
  called synchronously from any thread: there is no executor.
- The secure channel: five lanes, QUIC raced against TLS/TCP, 16 KiB fragments, windows of 32 MiB +
  5 (video) and 16 MiB (bulk), 64 messages per lane, weighted TCP order giving video 3 turns of 16,
  and per-connection limits of 128 messages and 64 MiB, of which 16 messages / 4 MiB are reserved
  for lane 0 (`crates/streamcore-transport/src/multiplex.rs:14-25,251-279`).
- Binary kinds: control `0x01`, ping `0x02`, pong `0x03`, video `0x10`, audio `0x11`, input `0x20`,
  terminal `0x30`, file `0x40` (`protocol/BINARY.md:120`; enforced by `Frame::valid_kind`,
  `crates/streamcore-media/src/binary.rs:399-401`). Every kind except video, audio, terminal and
  file defaults to lane 0 (`multiplex.rs:63-68`); a consumer's `LanePolicy` places its control
  messages (`:46-70`).
- Sessions (`crates/streamcore-session/src/lib.rs`): `attach` supersedes the previous generation;
  detached sessions resume for 120 s, at most 8 per peer and 256 in all (`:11-13`).

### 2.3 The reference host path today

The reference host (MyCNC on macOS) confines all stream state to one serial queue per stream, which
3.5 keeps. ScreenCaptureKit (SCK) captures NV12 `420v` at `queueDepth` 5 and canvas members BGRA at
depth 3. Recovery after a host-detected loss waits 0.5 s, then runs at most 2 per second; a client
keyframe request recovers at once.

### 2.4 Measured evidence (VM measurements only)

Every figure here is a **VM measurement** from the reference implementation's VM qualification,
not physical hardware; the reports stay in its repository. None closes a gate.

**Pacing, VM** (three runs; 512 px, H.264, up to 60 fps; host load 21–26, uncontrolled). Click →
decoded (one client clock, stops at the decoder): p50 19.9–24.5 ms, p95 38.5–42.8 ms in two runs and
100.1 ms in the third, max 106–154 ms; decoded-interval p95 33.2–59.5 ms; skipped 8.5–17.3%. The
third run's source animation itself stalled (source interval max 108.6 ms). The four-colour test
cannot say which stage lost a frame (9.6 replaces it).

**Transport probe, VM** (NAT, synthetic 120 Hz × 96 KiB video,
`crates/streamcore-transport/examples/scheduling_probe.rs`, method in `ci/scheduling/README.md`):
video RTT p95 24–26 ms on QUIC with 1-MiB bulk bursts, 14.4–23.5 ms with 256-KiB bulk, 4.1 ms
without bulk, 1.5–2.5 ms on TLS/TCP with bulk; QUIC receive-gap p95 10.4 ms without bulk and
14.1–23.3 ms with it, against an 8.3 ms send interval. Raising video's QUIC priority alone did not
help.

**Failover, VM** (emulated path changes on NAT): an announced change resumes in median 10 ms, p95
≤ 21 ms; over a soak of 119 changes, resume median 24 ms, max 30 ms, and video decoding again at p95
272 ms, max 383 ms; a silent break recovers after 10.7–10.8 s (QUIC idle timeout); TLS/TCP soak
recovery p95 292 ms. Resume is cheap; the time goes into what follows it (8.9).

### 2.5 Invariants every backend keeps

Qualified by the reference implementation; the portable rules are in [capture
pipeline](CAPTURE-PIPELINE.md) and [sessions](SESSIONS.md). The port moves their tests first.

1. **Admission:** at most 3 frames in flight per stream, counted across encoder replacement.
2. **Latest frame:** one retained raw frame per source; stop and reconfigure clear it.
3. **Recovery:** the retained image is re-encoded as a keyframe, never while the sink is
   backpressured; `lost_seq` and `submitted_seq` fence it; a no-output completion is a loss; stale
   callbacks never restart recovery.
4. **Codec truth:** the announced codec is the one created; generation increases on every encoder or
   format change; config goes with every keyframe.
5. **Colour:** primaries, transfer and matrix reach the encoder's VUI; non-VideoToolbox encoders set
   the range explicitly and the VUI normaliser (3.4) checks it; padding is black. A change in the
   planned output `ColorTags` (the source tags after the plan's per-client mapping, 3.3) replaces
   the encoder session; a source change that maps to the same output does not. A session
   replacement always reports `config_changed` ([capture pipeline](CAPTURE-PIPELINE.md)), even with
   byte-identical parameter sets, so it starts a new generation whose first frame is an IDR. The
   executor plans the replacement on its next wake; `submit` never replaces a session (3.5). (New
   for the port: the always-`config_changed` rule and the deferred re-create are not yet qualified.)
6. **Lifecycle:** each op has a token and a 10 s timeout yielding `outcomeUnknown`; a capture whose
   stop failed or is unknown blocks a replacement; one pending command; 35 s command timeout.
7. **Canvas:** a member without a first image is never shown as black (5 s deadline); a layout
   change fails with `stream.layoutChanged`; 2–8 members, ≤ 8192 px per edge, ≤ 7680×4320.
8. **Security and validation:** pinned mutual TLS 1.3 without resumption, and no name, serial or
   cable authorizes a peer ([secure channel](SECURE-CHANNEL.md)); `streamcore-video` validates
   everything before native allocation.

## 3. Architecture

### 3.1 Layering and crates

```
 streamcore-video, per stream: StreamExecutor (serial)      streamcore-transport
 ┌──────────────────────────────────────────────────┐   ┌──────────────────────────────┐
 │ lifecycle, validator, latest slot, permits,      │──▶│ lanes 0..4, credits; QUIC or │
 │ pacer, recovery, ABR, canvas, packetizer, sink   │   │ TLS over TCP/adb/usbmux/AOA  │
 └─────▲ post ────────── call ▼ (executor only) ────┘   └──────────────────────────────┘
 ┌─────┴────────────────────────────────────────────┐   client: VideoReceiver (Rust)
 │ native: capture, convert, compose, encode        │     ▲ post │ call ▼ Decoder (native)
 │ (IOSurface / D3D11 texture / DMA-BUF stay here)  │
 └──────────────────────────────────────────────────┘
```

New crates (every dependency AGPL-3.0 compatible):

- `crates/streamcore-video`: safe Rust (`unsafe_code = "deny"`): the executor, policy, traits and
  UniFFI exports, on `streamcore-media` and `streamcore-transport`. The one exception is the
  per-frame C ABI's `streamcore_video_encoded`, which reads the caller's bytes (3.5 "Overhead"). It names no
  product type: product differences are behavioural fields of `StreamConfig` (3.3).
- `crates/streamcore-usb`: host-side USB pipe providers (adb, usbmux, AOA over `nusb` on Linux) and
  the AOA link layer shared with the macOS and Android adapters (8.4).
- `crates/streamcore-video-windows` (P5) and `crates/streamcore-video-linux` (P7).

The platform crates and `streamcore-usb` opt out of `forbid`; the item-level allows of
`crates/streamcore-input-ffi/Cargo.toml:18-27` do not scale to windows-rs. Proposal: crate-level
`unsafe_code = "allow"` with `unsafe_op_in_unsafe_fn`, `undocumented_unsafe_blocks` and clippy `all`
denied, unsafe confined to per-API modules (Q1). Apple and Android adapters belong to each consumer;
identical ones may become StreamCore packages like `StreamCoreInput` (Q12).

### 3.2 Surfaces and leases

```rust
pub type SurfaceId = u64;   // index + generation into a native slot table; 0 is invalid
pub enum SurfaceKind { IoSurface, D3d11Texture { adapter_luid: u64 }, DmaBuf { fourcc: u32, modifier: u64, planes: u8 }, CpuMapped }
pub enum GpuFence { None, MtlSharedEvent { handle: u64, value: u64 }, D3d11Fence { shared_handle: u64, value: u64 }, SyncFile { fd: i32 } }
pub struct SurfaceInfo { id: SurfaceId, kind: SurfaceKind, format: PixelFormat /* NV12, BGRA, P010, FP16 */,
    width: u32, height: u32, color: ColorTags /* primaries, transfer, matrix, full_range */,
    capture_ns: u64 /* H1 */, source_present_ns: Option<u64> /* H0 */, dirty: Option<Vec<Rect>>, rotation: Rotation, fence: GpuFence }
```

Every `*_ns` value is host-monotonic nanoseconds, converted natively (mach ticks × timebase; QPC
ticks × 10⁹ / frequency, so WGC's 100 ns `SystemRelativeTime` × 100; `CLOCK_MONOTONIC`; see
[VIDEO-THREADING.md](VIDEO-THREADING.md#clocks)). Ownership rules:

- The producer owns a surface. Dropping Rust's `SurfaceLease` calls `release(id)` exactly once: on
  the stream executor while it exists, else on the posting thread (after `Closed`, 3.5). Every
  backend's `release` is therefore thread-safe, non-blocking and a leaf call (3.5). A surface is
  valid only on its adapter.
- **Single target:** one latest frame plus permits in flight (≤ 3), at least one below the capture
  pool depth (SCK `queueDepth` 5).
- **Canvas members:** at most 2 leases each (latest plus one being composed). The reference host
  captures members at `queueDepth` 3, which would leave SCK one buffer, so the GPU path uses member
  `queueDepth` 4–5. A backend whose pool is too small copies out into its own ring (WGC).
- **Composite pool:** at least permits + 2 surfaces, more than 1 retained + 3 permits.
- Consumers of a surface wait on its fence on the GPU. `stop` and `reconfigure` clear every lease
  before the backend's stop completes; slot generations make a late release a no-op.
- Trait records carry metadata only. Encoded bytes enter Rust once, through
  `VideoSinkHandle.submit_encoded(token, bytes)`; `Decoder::decode` gets an opaque `SampleHandle`
  whose bytes the adapter copies once (P0 rewording, 10).

### 3.3 Capability negotiation and the stream plan

1. **Host probe** at start and on a GPU or display change. `CaptureCaps`: target kinds,
   `window_fit`, `live_region_move`, cursor modes, surface kinds, pool depth, virtual displays.
   `EncoderCaps` per codec: LTR, temporal layers, intra refresh and low-latency rate control, each
   `Confirmed | Requested | Unsupported` from a probe session; limits, inputs, force-IDR, dynamic
   rate, measured `min_inputs_before_output` and the VUI check.
2. **Client caps** (`MediaCaps`, defined by StreamCore, carried in the consumer's control protocol
   before the first stream start): codecs, profiles, decode limits, panel refresh range, gamuts,
   maximum transfer and understood frame extensions.
3. **Per-stream plan** from host caps, client caps, the validated `StreamConfig` and the medium:

```rust
pub enum StreamMode { View, Control }        // required on every stream start
pub enum OnSourceGone { Fail, FallBackToMain }
pub struct CaptureSpec { target: CaptureTarget, mode: StreamMode, pixel_width: u32, pixel_height: u32,
    min_interval_ns: u64, format: PixelFormat, color: ColorRequest, cursor: CursorMode, queue_depth: u8 }
pub struct StreamConfig { max_long_edge_px: Option<u32>, max_fps: u32, quality: f32, bitrate_ceiling_bps: Option<u64>,
    codec: Codec, shows_cursor: bool, on_source_gone: OnSourceGone }
pub struct StreamPlan { mode: StreamMode, codec: Codec, encode_px: (u32, u32), fps: u32, bitrate_bps: u64,
    convert: ConvertPath, encoder: String, color_out: ColorTags, extensions: Vec<FrameExtension>, permit_target: u8,
    recovery: Vec<RecoveryStep>, on_source_gone: OnSourceGone, medium_cap_bps: Option<u64> }
```

`mode` keeps the reference host's behaviour. **View** never raises, fits or resizes a source
window; a window source must be on screen and an app source resolves to the app's front visible
window at start, else `stream.viewOnlyUnavailable`; fit and scale fail with `stream.viewOnly`.
**Control** allows window fit and raise where `CaptureCaps.window_fit` is set; the host app's own
windows can be viewed but not controlled (`stream.ownWindow`).

P3 output only when the client declares `p3` and can decode it (Android cannot; it gets BT.709). The
plan maps the transfer function as well as the primaries per client: Android's
`KEY_COLOR_TRANSFER` has no sRGB value, so an sRGB-tagged source goes to Android as
`COLOR_TRANSFER_SDR_VIDEO` (the BT.709 transfer), and the converter output and VUI match. The
permit target is `max(min_inputs_before_output, 1)` up to 3. The plan's features (`ltr`,
`intraRefresh`, `temporalLayers`, `timing`) go into the consumer's stream-started message.

### 3.4 Traits

Host traits are `#[uniffi::export(with_foreign)]` for Swift and Kotlin (full definitions move to
rustdoc in P1); Windows and Linux implement them in Rust. Every outbound call below except `release`
runs only on the stream executor (3.5); every `*Events` method only posts.

```rust
// Where a stream's executor runs. Implemented natively on Apple and Android, in Rust elsewhere.
pub trait SerialQueue: Send + Sync {
    fn wake(&self);                                  // run the port's drain() on this queue soon; never inline
    fn wake_after(&self, delay_ns: u64, timer: TimerToken);   // neither blocks nor takes an app lock
}                                                    // is_current is tracked in Rust (3.5)

pub trait CaptureBackend: Send + Sync {
    fn caps(&self) -> CaptureCaps;
    fn enumerate(&self) -> Result<SourceCatalog, MediaError>;
    fn observe_layout(&self, display: u32) -> Option<DisplayLayout>;
    fn open(&self, spec: CaptureSpec, queue: Arc<dyn SerialQueue>, events: Arc<dyn CaptureEvents>)
        -> Result<CaptureId, MediaError>;                                  // callbacks are delivered on `queue`
    fn start(&self, id: CaptureId, op: OpToken);                           // result via events.op_done
    fn update(&self, id: CaptureId, update: CaptureUpdate, op: OpToken);   // fps, live region origin, cursor
    fn stop(&self, id: CaptureId, op: OpToken);
    fn release(&self, surface: SurfaceId);                                 // every release: thread-safe leaf (3.5)
}
pub trait CaptureEvents: Send + Sync {               // Rust; post-only
    fn frame(&self, id: CaptureId, member: Option<u8>, surface: SurfaceInfo, status: FrameStatus);
    fn op_done(&self, op: OpToken, result: Result<(), MediaError>);
    fn source_gone(&self, id: CaptureId, reason: SourceGone);
}
pub trait Converter: Send + Sync {                   // records GPU work and returns at once
    fn convert(&self, input: SurfaceId, job: ConvertJob) -> Result<SurfaceInfo, MediaError>;
    fn release(&self, surface: SurfaceId);
}
pub trait Compositor: Send + Sync {                  // events: composed(surface), surface_released()
    fn configure(&self, plan: CanvasPlan) -> Result<(), MediaError>;       // pool >= permits + 2
    fn compose(&self, members: Vec<MemberSurface>) -> Result<ComposeResult, MediaError>;  // Composed | PoolExhausted
    fn release(&self, surface: SurfaceId);
}
pub trait EncoderFactory: Send + Sync {
    fn probe(&self, adapter: Option<u64>) -> Vec<EncoderCaps>;
    fn create(&self, spec: EncoderSpec, events: Arc<dyn EncoderEvents>) -> Result<Arc<dyn Encoder>, MediaError>;
}
pub trait Encoder: Send + Sync {
    fn info(&self) -> EncoderInfo;                   // codec created, hardware, low latency confirmed
    fn submit(&self, frame: SurfaceId, opts: SubmitOpts, permit: Arc<VideoWorkPermit>) -> SubmitResult;  // never blocks or replaces the session
    fn set_rate(&self, update: RateUpdate) -> Result<(), MediaError>;
    fn invalidate(&self) -> InvalidateOutcome;       // the one allowed wait (3.5): pending output, bounded in time; Completed | TimedOut (hung)
}
pub trait EncoderEvents: Send + Sync {               // Rust; post-only; any thread
    fn encoded(&self, frame: EncodedFrame);          // metadata + payload token
    fn dropped(&self, submit_seq: u64, reason: DropReason);   // includes no-output completions
    fn input_ready(&self);                           // after a Busy submit
    fn session_failed(&self, error: MediaError, recoverable: bool);  // the session died with no frame to carry the error
}
```

`EncodedFrame` carries `submit_seq`, `pts_ns`, keyframe and config flags, bitstream form (`Avcc4 |
AnnexB`), the payload token, `discardable`, `temporal_id`, LTR info and timings. Window fit (macOS)
and an optional `VirtualDisplayBackend` are separate traits whose results are posted.

`VideoSink` (from P2, internal to Rust, replacing the consumer's video send path) puts video on lane
1 as kind `0x10`, applies 6.4, and reports `SinkState { unsent_frames, oldest_unsent_age_ms,
unconsumed_frames, credit_rtt_ms, backpressured }` to the pacer, recovery and ABR. `push` runs on
the executor right after `Encoded` (R1); in P1 the `VideoOut` hop took its place (3.5). Shipped
in P2: `StreamParts::output` (`VideoOutput::channel(SecureChannel, SinkPolicy, capacity)`) replaces
`VideoOut`; the switches of 11.1 are `SinkPolicy` fields, and `StreamController::sink_state()`
reads the published `SinkState`.

```rust
// Consumer observers. Delivered by posting a job to the consumer's queue, never run on an executor.
pub trait ObserverQueue: Send + Sync { fn post(&self, job: ObserverJob); }   // non-blocking
// ObserverJob: Started(plan), Failed(code), OpResult(token, result), SourceGone, LayoutChanged(epoch), Stats(snapshot)
```

Shared utilities for non-Apple encoders: Annex-B → length-prefixed conversion, the avcC/hvcC
builder, and an **SPS/VPS VUI normaliser** that checks the colour description against `ColorTags`
and writes zero-reorder `bitstream_restriction` (else some decoders buffer until the DPB fills),
with a contract test per encoder.

### 3.5 Stream executor and threading

**Executor** (this section is normative). Every stream (single target, canvas, or client decode
stream) has exactly one `StreamExecutor`: a FIFO mailbox plus a deadline heap, drained by one thread
at a time on the stream's `SerialQueue`. The executor owns all stream state, which is not `Sync`:
the latest slot, the pacer, packetizer state, the `submitted_seq` / `delivered_keyframe` /
`lost_seq` fences, recovery state, `StageTrace`, the lifecycle machine and canvas members. Nothing
else reads or writes it. The executor never runs on `streamcore-network` and never blocks: no
sleeps, no `.await` on I/O, no synchronous waits. The one allowed wait is `Encoder::invalidate`'s
wait for the encoder's pending output (on VideoToolbox, `VTCompressionSessionCompleteFrames`). It
covers at most the 3 frames in flight, cannot deadlock because encoder output only posts
(reentrancy rule 4 below), and is bounded in time: `invalidate` returns after at most 500 ms
(proposed, Q9; an adapter whose platform call has no timeout runs it off the executor and waits
with one). Outputs that arrive later carry the old generation and are discarded as stale events
after their permits are released; a timed-out invalidate reports `EncoderFailed`.

**The capture queue is the executor.** Capture callbacks are delivered on the stream's
`SerialQueue`, so convert and submit run on the capture queue by construction; a due submit is an
internal executor job scheduled by a post or a timer, never a public call.

**Inbound calls only post.** `CaptureEvents::*`, `EncoderEvents::*`, `DecoderEvents::*`, compositor
events, sink receipts (posted by futures on `streamcore-network`), permit releases and commands
enqueue an event and return in O(1), touching no stream state and calling no native code except
`SerialQueue::wake`.

**Inline drain, frames only.** When `frame` arrives on the executor's own queue and no drain is
active, only admission, convert and submit for that frame run inline, so an SCK callback reaches
submit with no hop. Every other job (backend stop or update, encoder re-creation or invalidate,
timers, observer posts) waits for the next wake, so nothing calls back into SCK from its own
callback. `is_current` is a per-executor thread-local marker set during a drain, not a
queue-specific key. A newer frame for the same capture and member replaces an undelivered one.
Every event carries its identity (encoder generation, capture id, op token, timer token); stale
events are discarded after their permits and leases are released.

**Colour or format change.** Admission maps each frame's `SurfaceInfo.color` through the plan's
per-client mapping (3.3) and compares the mapped output tags and the format with those the current
encoder was created with; the raw source tags are never compared with the encoder's. Only a change
there keeps the frame as the latest without submitting it and schedules a re-create for the next
wake: `invalidate` the old encoder, `create` the new one, mark its first output `config_changed =
true` (a new generation, 2.5) and submit the retained frame as an IDR. `submit` never replaces a
session.

**Outbound calls run only on the executor, during a drain:** `CaptureBackend` open, start, update
and stop; `Converter::convert`; `Compositor` configure and compose; `EncoderFactory::create`;
`Encoder` submit, `set_rate` and `invalidate`; `Decoder` calls and present; `VideoSink.push` (P1:
`VideoOut::send`, removed in P2); and `ObserverQueue::post`. Synchronous results are returned as values
(`SubmitResult`, `ComposeResult`), never as callbacks.

**Leaf calls** may run on any thread; each uses atomics or at most one short, non-nested Rust lock,
released before any native call, and never waits on an executor: `post` (which calls
`SerialQueue::wake` after dropping the mailbox lock); permit release and its Drop;
`VideoWorkQueue.close`; `VideoSinkHandle.submit_encoded`; every backend's `release(surface)`
(thread-safe, non-blocking; on the executor while it exists, 3.2); the sink's `revoke` and enqueue
(enqueue may take `streamcore-transport`'s internal locks inside the sink lock, never the reverse);
and reads of an atomic snapshot the executor publishes (`SinkState`, the input destination). `wake`
and `wake_after` never block and take no app locks.

**Permits.** P1 restructures `VideoWorkPermit` around an atomic `released` flag and an atomic
in-flight count, with no nested locks. Every release posts `PermitReleased` to the owning executor,
which then submits the retained frame if one is waiting: on a still screen nothing else would
retry it (R1).

**Consumer observers** run only as jobs posted to the consumer's `ObserverQueue` (3.4), never on the
executor, so an observer may call anything, including `close_and_wait`.

**Authority.** Screen authority is a revocation token held by the stream's sink. `revoke()` and the
sink's enqueue take the sink's own lock and enqueue checks the token under it, so **once `revoke()`
returns, no frame for that authority is enqueued.** The executor also checks it before each
outbound call. No authority lock wraps a call into Rust.

**P1 send hop (`VideoOut`).** P1 keeps the consumer's existing send path. Right after `Encoded`,
the executor calls the foreign trait `VideoOut::send(meta, bytes, receipt)` with no lock held; the
adapter takes its sink lock (which its `revoke()` also takes), checks the token and calls the
consumer's send. Every completion, a synchronous refusal included, goes to the `SinkReceipt`;
refusals and accepted keyframes are posted back (an accepted delta changes nothing). A send path
holding its own lock across its enqueue into `streamcore-transport` is an allowed leaf. From P2, `VideoSink` replaces `VideoOut` (D2):
its lock is taken before any transport lock, never the reverse; an idle sink enqueues inside `push`
on the executor; later frames are enqueued by the previous frame's write completion on
`streamcore-network` ([threading](VIDEO-THREADING.md#authority-and-the-sink)).

**Reentrancy rule.**

1. **No re-entry from a callback.** Code running inside an outbound call, or in a native callback
   triggered by one, must not call back into the same stream synchronously except through `post` and
   the leaf calls. A completion is always posted, even when it is known at once.
2. **No lock held across the boundary, in either direction.** Native code holds no lock (`NSLock`,
   `Mutex`, `os_unfair_lock`, `synchronized`, an authority block) while it calls into Rust, except
   around the leaf calls named above. Rust holds no `MutexGuard`, including the executor's own,
   while it calls native code.
3. **No synchronous wait on an executor** from any executor or `streamcore-network` worker, or from
   anything an executor may wait on: callbacks, encoder or decoder output threads, or any native
   method reached from an outbound call. (`Encoder::invalidate` waits on an encoder, not an
   executor; it is the one exempt wait, see rule 6.)
4. **Encoder and decoder output threads touch no stream state.** They may copy bytes, do pure
   conversions (Annex-B to AVCC, avcC/hvcC), call `submit_encoded` and `post`.
5. **Permits** are acquired on the executor before `submit` and released when the sink accepts the
   frame or by Drop wherever native code drops them; every release posts `PermitReleased`.
6. **Debug builds assert** `is_current()` on every outbound call and state access, and assert on
   every StreamCore blocking entry point (such as `close_and_wait`) that the thread is on no
   executor and no `streamcore-network` worker (a thread-local flag set by every drain and by the
   runtime's thread start). `Encoder::invalidate` is the one exempt wait (Executor, above).

**Close.** `close(completion)` (Swift and Kotlin `async close()`) is the default: it revokes the
sink token, posts `Close` and returns at once; the completion runs on the `ObserverQueue`.
`close_and_wait(timeout)` is only for synchronous native teardown: on an executor or network worker
it asserts in debug builds and errors in release builds (rule 6); elsewhere it waits at most
`timeout` (default 10 s) and returns `Closed` or `StopUnknown`, the sink already revoked. Tearing
down several streams posts every close first, then waits.

**Closing and Closed.** On `Close` the executor stops admitting, calls the backend's `stop` and the
encoder's `invalidate`, and keeps draining in release-only mode (late frames, outputs, receipts and
timers only release their leases and permits). Once `op_done` has arrived and `invalidate` has
returned, it enters `Closed`, completes the close, and drops its references to the backend,
encoder, sink and queue; event ports hold the executor weakly, so this ends the cycle. A post to a
port whose executor is gone releases any lease or permit it carries on the calling thread (a leaf
call, 3.2) and drops the event. After the close completes, no push or outbound call is in progress
and none follows.

**Fences.** `submitted_seq` advances on the executor when `submit` returns `Submitted`; `Busy` and
`Failed` consume no sequence, so a refused submit leaves no gap; gaps from encoder drops, shedding
(6.4) and handover drops (8.8) are kept, never renumbered ([capture pipeline](CAPTURE-PIPELINE.md)).
`Encoded`, `Dropped` and the sink's reports (P1: `SinkReceipt`) update `delivered_keyframe` and `lost_seq` as the reference
host does. Recovery requests read `submitted_seq` on the executor; recovery timers are `wake_after`
deadlines; the packetizer runs only on the executor, in event order.

**Per platform.**

- **macOS host:** a per-stream serial `DispatchQueue` (`.userInteractive`), the `sampleHandlerQueue`
  of every `SCStream` of the stream, canvas members included; `wake` is `queue.async`. The VT output
  handler hands each output over (`submit_encoded` and `Encoded` as one C call that copies the
  bytes once, from the sample's block buffer). Window fit, the settled-size poll,
  Accessibility, Metal completions and pool releases post.
- **Windows host:** one Rust thread per stream (MMCSS "Capture") waiting on the mailbox and a
  high-resolution timer and pumping a `DispatcherQueue` (4.2). **Linux host:** the PipeWire
  `process` thread and encoder outputs post. **Android client:** the decode stream's
  `HandlerThread` (`THREAD_PRIORITY_URGENT_DISPLAY`), shared by `MediaCodec`, the frame-rendered
  listener and `Choreographer`. **Apple clients:** a serial `DispatchQueue`.

**Overhead.** Per frame on Apple: `frame`, `submit`, `submit_encoded`, `encoded`, plus `convert`
when scaling. Gate: added overhead p99 ≤ 50 µs per frame at 120 fps (P1), else a C-ABI fast path;
the H4 → H5 gap includes the hop and is gated (9.6). Through UniFFI the gate failed: every call
clones a handle, every record, enum, option and byte string is serialized into a buffer that the
other side parses and frees with further calls, and foreign callbacks look their object up in a
locked map; between frames the caches are cold, so one frame's calls cost a Swift backend about
90 µs at p50 and over 200 µs at p99 in the reference VM. A foreign backend therefore makes the
per-frame calls (frame, submit, surface release, encoder output, wake, drain, send and its
completion) through a small C ABI with primitives only and keeps UniFFI for control
(`include/streamcore_video_native.h`, [threading](VIDEO-THREADING.md#the-per-frame-fast-path)).
Rust backends call the traits directly and are unaffected.

**Tests (P1 gate):** the reference host's single-queue pipeline tests on mock traits, plus
seeded-shuffle, reentrancy, close (a stuck `invalidate` returns within its bound), a stop or
reconfigure arriving during an inline drain, and colour re-create cases (re-created on the next
wake, never inside `submit`; an sRGB source on an Android-mapped plan gives exactly one encoder
session).

### 3.6 Lifecycle

`StreamController` ports the reference host's state machines (capture lifecycle, command layer,
group capture, canvas start-up) with their tests.

```
Idle ─start→ Starting ─op_done(ok)→ Running ─update→ Updating ─ok→ Running
  ▲              │ 10 s → StartUnknown (capture retained)               │
  │              └ err → Failed(stream.*) → Stopping                    ├ source_gone → per on_source_gone
  └── Stopped ←ok─ Stopping ─10 s→ StopUnknown (retained; blocks replacement) ─err→ StopFailed
```

- Every op carries an `OpToken` (u64 + stream epoch); stale tokens are ignored.
- **Size, codec or colour change:** new encoder, new generation, config + IDR; old permits still
  count. During a resize the encode size is kept and the converter letterboxes; Rust re-plans once
  the size is stable for a settle time (proposed 600 ms, Q9), at most one restart per second.
- **Rate-only change:** `set_rate` and `capture.update`, no restart. **Region:** a normalized,
  nestable rect; a move is a live update, a resize a size change.
- **Layout epoch:** incremented on every layout change; frames carry it (9.4) and client input
  carries the epoch of the last presented frame (last 4 kept).
- **Source gone:** `Fail` fails the stream; `FallBackToMain` switches to the main display.
  **Suspend** (locked, login window, display asleep): stop submitting, keep the stream, send the
  retained frame as an IDR on resume.
- **Handover** (8.8): `detach_transport()` parks, `attach_transport(sink)` rebinds. **Close** (3.5)
  leads from any state through `Closing` to `Closed`; a stop that times out reports `StopUnknown`.

### 3.7 Errors and wire codes

`MediaError` is `InvalidSettings`, `Unsupported`, `PermissionDenied`, `SourceGone`, `DeviceLost`,
`OutOfResources`, `EncoderFailed { recoverable }`, `Timeout` or `Internal`. Reactions: `Unsupported`
at plan time picks the next encoder or codec; `DeviceLost` rebuilds converter and encoder with a new
generation and IDR (more than 3 in 10 s fails the stream); a recoverable encoder failure runs the
recovery ladder; `OutOfResources` drops the frame and retries when a surface is released, never on a
fixed timer; `Timeout` enters an `*Unknown` phase with the capture retained.

Wire codes are ported unchanged from the reference host into a `protocol/` file next to
`BINARY.md` in P1; the only new one in v1 is `session.stale` (8.7). Commands: `stream.config`,
`stream.limit`, `stream.busy`, `stream.cancelled`, `stream.timeout`, `stream.command`,
`stream.missing`. Mode: `stream.viewOnly`, `stream.viewOnlyUnavailable`, `stream.ownWindow`.
Sources: `stream.target`, `stream.dimensions`, `stream.content`, `stream.viewport`, `stream.fit`,
`stream.scale`, `stream.restore`. Capture and encode: `stream.start`, `stream.stop`,
`stream.update`, `stream.capture`, `stream.output`, `stream.encoder`, `stream.encode`,
`stream.sequence`. Canvas: `stream.firstFrame`, `stream.memberUnavailable`, `stream.layoutChanged`.
Permissions: `permission.*` ([input model](INPUT-MODEL.md)).

### 3.8 Bindings per platform

Apple: Swift through UniFFI foreign traits, an XCFramework built like
`scripts/build-streamcore-input-apple.sh` (`seq` ↔ VT `sourceFrameRefCon`). Android: `Decoder` in
Kotlin, built like `scripts/build-streamcore-input-android.sh`, first run in P3b (`seq` ↔
`presentationTimeUs`). Windows and Linux: Rust. UniFFI is pinned at `=0.32.2` (`Cargo.toml:19`) and
nothing uses `with_foreign` yet, so P1 first adds a Swift and Kotlin async round-trip test; if it
fails, `ByteDuplex` becomes a sync callback pair.

### 3.9 Compositor (combined canvas)

Capture is per monitor on every OS, so every host needs a compose stage for a canvas of several
displays. Today the reference host composes BGRA tiles on the CPU (arrival-triggered, rate-limited,
a pool of three, an exhausted pool retrying one interval later) and VideoToolbox converts to YUV.

- Rust keeps all policy: layout validation, tile maths, all-or-nothing group start and stop, the 5 s
  first-image deadline, 250 ms re-observation, cadence and input mapping.
- The native `Compositor` only draws the latest lease per member into an NV12 surface from a pool of
  at least permits + 2, gaps black: Metal on macOS (gamut and matrix in the shader, `MTLSharedEvent`
  completion), a D3D11 pass on Windows. `PoolExhausted` retries when a pool surface is released,
  not on the next member arrival: on a still screen nothing arrives.
- The composite's H0 is that of the **newest changed member**: an idle member's image can be
  seconds old and would make every composite look stale to 6.4. The member age spread is a metric.
- Estimated 0.5–1.5 ms on Apple silicon for a 5K-wide canvas; the CPU path is benchmarked first, and
  canvas and colour qualification re-run.

**As shipped (P2).** `StreamController::new_canvas(parts, CanvasParts { layout, compositor })`
runs a `CaptureTarget::Canvas` stream; `StreamController::new` keeps answering such a spec with
`Unsupported`, so a backend without a compositor keeps the consumer's own path. The layout
(`CanvasArrangement`, members in one global coordinate space with their scale) is validated with the
consumer's limits (2–8 members, no overlap, ≤ 8192 px per edge, ≤ 7680×4320); `canvas_size`,
`canvas_tiles`, `canvas_input_target` and `canvas_nearest_member_point` are exported for the
consumer. Rust opens one `Display` capture per member at its tile size, starts and stops them
as one group (a failed start stops only the members that started; a failed member stop keeps
that member running, `StopFailed`), acknowledges the start only once every member delivered an
image within 5 s (`stream.firstFrame`), observes the layout before allocating anything, when the
first images are in and every 250 ms through `CaptureBackend::observe_layout`
(`stream.layoutChanged`: origin, size, scale or `DisplayLayout::rotation` differ; a mirrored or
cloned display is `None`), and fails on a blank, suspended or gone member (`stream.memberUnavailable`). A member
image marks the canvas dirty; a composite is drawn at most once per frame interval and is due for
submit at once. `Compositor::configure` gets a pool of 5 (the largest permit target + 2);
`PoolExhausted` (or `OutOfResources`) waits for Rust to release a composite (a fallback timer of
one interval when Rust holds none). The composite's H0 and H1 are the newest changed member's;
`StatsSnapshot` counts composites, pool waits and member images and reports the member age
spread. Under frames.v4 each canvas start sends `layoutInfo` (the members' union filling the
picture, upright: its `rotation` is 0, since the compositor turns any member surface whose
`SurfaceInfo::rotation` is set) for a new layout epoch, which frames carry in the `content` TLV
when the plan selects `Content`. `DisplayLayout::rotation` is the clockwise turn from the panel's
native scan-out to the desktop; its bounds are already rotated, so it serves layout comparison
and input mapping, not the picture. A single target's rotation reaches the consumer as
`layout_changed` (a new epoch), never as a gone source. The compose call itself is UniFFI (one per composite), not part of the per-frame C ABI.

**Other consumer surfaces.** Consumer previews (one-shot stills, long edge ≤ 320 px) stay outside
`streamcore-video` in v1. Audio keeps its own lane and path; only the handover (8.8) touches it.

## 4. Host backends

### 4.1 macOS (first implementation)

- `CaptureBackend`: `SCStream` per target, NV12 `420v`, `queueDepth` 5; H0 =
  `SCStreamFrameInfo.displayTime`; `dirtyRects`; live `sourceRect` for regions; window fit only in
  Control mode, off the executor. `Converter`: identity for NV12 single targets, else
  `VTPixelTransferSession` or Metal. `Compositor`: Metal (3.9).
- `Encoder`: `VTCompressionSession` with `EnableLowLatencyRateControl`, `RealTime`, no reordering,
  `MaxFrameDelayCount = 0`, read-back of hardware and low-latency state, a 100–200 ms
  `DataRateLimits` window, `MaxAllowedFrameQP`, and LTR or `BaseLayerFrameRateFraction` only where
  the per-codec probe confirmed them (H.264 first). The GOP is unbounded, so recovery stays
  request-driven.

### 4.2 Windows (decided in P5)

Fixed now: WGC into D3D11, one shader pass (scale, BGRA → NV12 with explicit matrices, cursor,
canvas tiles), the MF hardware MFT (`MFTEnum2` scoped by adapter LUID, low-latency mode, fed only on
`NeedInput`), NVENC as first vendor tier, the Microsoft software MFT (≤ 1080p30) as fallback,
Desktop Duplication or GDI (on their own thread) when WGC shows nothing, and dual-stack sockets
(`IPV6_V6ONLY = false`, tested on Windows from P3). The P5 spike settles:

- **Hybrid laptops.** Before build 26100 WGC fires at the DWM rate, and with WGC on the encoder
  adapter DWM copies every composed frame across adapters: about 2.1 GB/s at 2560×1440 and 144 Hz
  (illustrative). P5 measures it, prefers `MinUpdateInterval`, and compares a D3D12 cross-adapter path.
- **No zero-copy claim.** Some MFTs accept only their own allocator's samples; the budget includes
  one GPU copy, and the probe records per encoder whether it ran.
- **Threading.** The executor thread creates the WGC frame pool with `Create` on its
  `DispatcherQueue`, so `FrameArrived` fires on the executor (`CreateFreeThreaded` would use
  thread-pool threads). MF and NVENC outputs post.
- Codec per adapter by measurement; on Intel Gen9-class iGPUs H.264 (VDENC) over EU-assisted HEVC.

### 4.3 Linux (P7) and CPU fallback

Linux: portal ScreenCast → PipeWire DMA-BUF → VAAPI, or EGL → CUDA → NVENC; designed in P7. Every
host has a CPU fallback (GDI/libyuv/MS software MFT, VT software, OpenH264), declared as
`SurfaceKind::CpuMapped` so plans and reports show the tier.

## 5. Client decode and present

```rust
pub trait Decoder: Send + Sync {
    fn configure(&self, codec: Codec, config: SampleHandle, color: ColorTags, queue: Arc<dyn SerialQueue>,
                 events: Arc<dyn DecoderEvents>) -> Result<DecoderInfo, MediaError>;
    fn decode(&self, sample: SampleHandle, meta: SampleMeta) -> DecodeResult;   // Accepted | Busy | Failed
    fn flush(&self);
    fn set_mode(&self, mode: PresentMode);                                     // Responsive | Smooth
}   // DecoderEvents (post-only): decoded(seq, t_ns), presented(seq, t_ns, estimated), failed(error)
```

The contract is R2: each frame is presented when decoded; if several wait for one vsync only the
newest is shown (R4). Rust owns reassembly, `VideoReceiver`, the two-frame decoder-queue bound
(then flush + keyframe request), feedback and clock sync.

- **Apple:** today samples go straight into `AVSampleBufferDisplayLayer` (C5 an estimate). From P2
  `VTDecompressionSession` → the same layer (exact C3); P6 A/B-tests Metal + `CAMetalDisplayLink`
  (exact C5). LTR is acknowledged after decode, never on submit.
- **Android:** async `MediaCodec` on the executor's Looper onto a `SurfaceView`; `FEATURE_LowLatency`
  hardware decoders preferred; `KEY_LOW_LATENCY` or vendor keys, retrying `configure()` with fewer;
  BT.709 colour keys (3.3); Responsive requests the top refresh rate; C5 from the frame-rendered
  listener, calibrated per device.
- **Desktop clients (later):** D3D11VA → flip-model swap chain; VAAPI/NVDEC → Wayland.

## 6. Pacing policy

### 6.1 Principles

Composition drives capture, never a timer loop; no stage waits for a successor frame (R1, R2); drop
before encode, shed after encode only as a last resort; nothing is sent while the screen is idle.
Clients run **Responsive** (default, present on decode) or **Smooth** (newest frame per vsync,
adaptive 0–1 frame jitter buffer).

### 6.2 In-flight limits

| Hand-off | Today (reference host) | Target, per stream | Enforced by |
| --- | --- | --- | --- |
| Capture pool | SCK 5; canvas members 3 | SCK 5; canvas members 4–5 on the GPU path; WGC 2–3, copied out at once | backend |
| Rust leases | n/a | ≤ 1 latest + permits; canvas member ≤ 2; composite pool ≥ permits + 2 | lease cap (3.2) |
| Frames in the encoder | ≤ 3 | `max(min_inputs_before_output, 1)`, +1 when encode p95 > 0.8 × interval, 3 hard cap | `VideoWorkQueue` target |
| Encoded, not yet written | per connection, all lanes: 96 receipts / 12 MiB disposable, 128 hard cap, backpressure at 80 / 8 MiB; multiplexer: 128 messages / 64 MiB, 16 / 4 MiB reserved for lane 0 | ≤ 1 frame, admitted only when the previous one is written | pacer admission + `VideoSink` |
| Written, not consumed by the peer | 32 MiB + 5 window, 64 messages per lane | ≤ max(2 frames, RTT × rate + 1 frame) | `VideoSink`, consumed-credit RTT |
| Relayed media (adb, usbmux), all non-interactive lanes | unbounded below the lane windows | ≤ RTT × capacity + 1 fragment per connection | multiplexer (8.6) |
| Kernel / quinn buffers | OS default / 10 MB | `TCP_NOTSENT_LOWAT` ≈ 1 frame (Windows: Rust-tracked queue, `SIO_TCP_INFO`, small `SO_SNDBUF`); quinn near BDP + 2 frames | transport |
| Client: waiting for the decoder | renderer readiness | ≤ 2, then flush + keyframe request | Rust receiver |
| Client: decoded, waiting to present | renderer internal | 1 (newest) at the next vsync | native renderer |

The old per-connection limits stay as memory safety nets only.

### 6.3 Refresh-aligned capture

H0 is the compositor present time. The pacer's due time advances by `target_interval` from the
previous due time (resynchronised after idle) and it accepts the first frame with `H0 ≥ due −
refresh_interval / 2`; earlier frames only update the latest slot. On a 144 Hz panel (6.94 ms) a 60
fps stream then gets 60 frames per second, each within half a refresh of its due time. Measuring
from the last accepted H0 instead accepts at ≥ 16.667 − 3.472 ≈ 13.19 ms, so every second refresh
(13.89 ms) qualifies: 72 fps, above the 60 fps target.

### 6.4 Post-encoder shedding in `VideoSink`

Per stream, before fragmentation:

1. A **discardable** frame still unsent when a newer one arrives is dropped; nothing else happens.
2. **Reference deltas are not shed on reliable lanes by default** (the handover drop, 8.8 step 3,
   is the one exception and is not counted as shed): drop-before-encode already holds the backlog
   to about one frame. A delta age bound of `max(100 ms, 3 × credit RTT)` from H5 ships only with
   confirmed LTR or temporal layers on that encoder; without them every shed delta forces an IDR.
   The shed rate is a gated metric (9.6).
3. Config and keyframes are never shed; a newer keyframe supersedes older queued frames.
4. A fragment being written is never pre-empted (at most one 16 KiB fragment of delay).
5. **Bulk token bucket**: while video is active, bulk is limited to (capacity − video rate) × 0.8 in
   the multiplexer. It ships **in the same release as `VideoSink`**: a consumer's file pump may pace
   on connection-wide backpressure that counts video; once video bypasses that path, only the bucket
   protects it.

**As shipped (P2).** Rule 2's "confirmed" is read from the plan: `recovery` contains
`LtrRefresh` or `extensions` contains `TemporalId`, which the planner sets only from confirmed
encoder caps; `StatsSnapshot::sink_age_bound` reports whether it is in effect. Rule 5's capacity
is the consumer's figure, else the plan's `medium_cap_bps`, else the sink's own estimate (the
largest write rate of a frame into an idle lane over 2 s, at least twice the video rate). That
estimate is **not measured against the link** and is reported as such
(`bulk_capacity_estimated`); the bucket keeps its 1 Mbit/s bulk floor. The inputs are only
useful if the channel runs the bucket (`TransportTuning::bulk_token_bucket` on before it was
created); otherwise `bulk_bucket_inactive` is set and the consumer should log it.

### 6.5 Recovery ladder

**LTR refresh** if the codec's caps confirm LTR and the client acknowledged a token after decode on
the current transport; then **intra refresh** where the encoder has it (not VT); then an **IDR** as a
per-frame option, never a session restart. For host-detected loss the first recovery fires at once
instead of after today's 0.5 s (client keyframe requests already do); later IDRs stay ≤ 2 per
second. LTR state clears on every generation change and transport switch. Discardable frames come
from two temporal layers where supported; missing discardable sequences do not invalidate the chain.

### 6.6 Rate adaptation

Delay-based and identical on every medium (TCP and USB relays hide loss): inputs are unsent age,
credit RTT above its windowed minimum, the client delay gradient, loss and quinn stats; ×0.85 when
queueing exceeds one frame, +5–8%/s otherwise, with a floor against bulk. Bitrate first, then fps,
then resolution (a new generation). The bitrate is `w·h·fps·(0.02 + 0.18q)` clamped to 2–80 Mbps,
the config ceiling and the medium cap; a 100–200 ms rate window with a frame-level maximum QP keeps
an IDR from blocking the link.

### 6.7 Skipped frames

Causes, none counted today: (a) no frame from the compositor; (b) the latest slot replaced while
every permit was busy; (c) encoder drop; (d) sender shed; (e) client flush. P0 counts them; E1 tries
`minimumFrameInterval` at 1/maxFPS, 1/(2·maxFPS) and 0 with the Rust pacer choosing, on an unloaded
physical host. No tuning against VM numbers alone.

## 7. Transport for video

### 7.1 Per link

Video uses reliable lane 1 everywhere: on QUIC with the 6.2 bounds and 7.2 tuning, on TLS/TCP when
UDP is blocked, and over TLS on USB byte streams (lossless; head-of-line blocking costs only
queueing). On a USB-C network interface (NCM) QUIC stays off until P3 qualifies UDP there.

### 7.2 QUIC tuning first

`quic_transport()` (`transport.rs:563-572`) sets only stream counts, keepalive 2 s and idle 10 s;
the rest is quinn's default (Cubic, 10 MB send window, `initial_rtt` 333 ms, an unused datagram
receive buffer). Tune windows near BDP + a few frames, lower `initial_rtt` on a LAN, evaluate BBR,
disable the datagram buffer, retry the priority experiment with bulk throttling, measure CPU per
Mbit, and re-run the scheduling probe before each change ships.

### 7.3 Datagram video (P8, conditional)

`video.datagram.v1` is a transport Hello capability under the session ALPN; datagrams use the
pinned connection's keys. Only the constraints are fixed now:

- **Only deltas go in datagrams.** Keyframes, config and LTR refreshes stay on reliable lane 1: an
  IDR of `n` ≈ 1,200-byte datagrams arrives whole with probability `(1 − p)^n`, so a 100 KB IDR (85
  datagrams) arrives 65% of the time at 0.5% loss and 18% at 2%.
- **Deadlines are size-aware:** `presentation slot + S × 8 / rate + RTT`; admission is per frame in
  Rust, with `videoFeedback` (9.4) replacing credits.
- **Gate G-DG:** lower H6 → C1 p95 by at least one RTT, less freeze time, skipped ≤ 1%, IDR delivery
  time and rate reported under 0.5% and 2% loss, at most +20% host CPU per Mbit on physical 5 GHz
  Wi-Fi.

## 8. USB transport

### 8.1 What exists and how clients dial today

StreamCore has no USB code. TLS, the transport Hello and the multiplexer are already generic over
`AsyncRead`/`AsyncWrite` (`crates/streamcore-transport/src/transport.rs:361,506`,
`multiplex.rs:498-522`); only construction is tied to TCP (`TransportKind`, `DialPath.address`,
`Session.peer_address`, `Incoming`: `transport.rs:25-35,70,285`; `ChannelPath.address`:
`channel.rs:42-48`; pairing: `pairing_transport.rs:73,89`). Facts the design depends on:

- **`dial` has no stagger rule.** 1–16 paths, each with a caller-chosen delay ≤ 2 s, then a 5 s
  deadline for TLS + transport Hello (`transport.rs:20-21,413-451`); the only 100 ms stagger is
  pairing's (`docs/SECURE-CHANNEL.md:76-77`). `connect_path` retries TLS/TCP with a 150 → 500 ms
  backoff (`:453-481`), which is wrong for a single-use pipe.
- **The reference client races routes in its own dialer**: one `dial` per route (QUIC, then TLS/TCP
  staggered), routes a few tens of milliseconds apart, first to finish wins, route set fixed.
- **Resume is not in the transport.** It travels in the control hello after `dial`. `SessionDaemon`
  is last-wins: any attach with a valid resume reference bumps the generation and supersedes
  (`crates/streamcore-session/src/lib.rs:111-174`), and superseding restarts streams today.
- **Pings before the control hello** are answered by the host; the client ignores non-control
  frames until its control hello is acknowledged and waits 5 s for it.

### 8.2 Transport API changes

```rust
pub enum TransportKind { Quic, TlsStream { medium: Medium } }        // TlsTcp == TlsStream { Tcp }
pub enum Medium { Tcp, UsbNet, UsbAdb, UsbMux, UsbAoa }
pub enum PeerLocator { Socket(SocketAddr), Usb { medium: Medium, device: String /* label only */ } }
pub struct DialPath { target: DialTarget, kind: TransportKind, delay: Duration }
pub enum DialTarget { Socket(SocketAddr), Pipe(PipeId) }               // Pipe: a pipe the device accepted
impl SecureListener {
    /// Host: hand over a pipe a provider opened. Same admission, trust fence and lane rule as accept().
    pub async fn accept_stream(&self, io: Arc<dyn ByteDuplex>, locator: PeerLocator) -> Result<(), ChannelError>;
}
impl SecureChannel { pub fn locator(&self) -> PeerLocator; pub async fn probe(&self, n: u8, spacing: Duration) -> ProbeResult; }
#[uniffi::export(with_foreign)]
pub trait ByteDuplex: Send + Sync {                                    // macOS IOUSBHost AOA only
    async fn read(&self, max: u32) -> Result<Vec<u8>, IoError>;       // empty = EOF
    async fn write(&self, data: Vec<u8>) -> Result<(), IoError>;
    fn close(&self);
}
```

- A `Pipe` path runs the TLS client handshake on the pipe, then the TLS/TCP path. **Pipe paths are
  excluded from `connect_path`'s retry loop**: one attempt, then the path fails.
- `accept_stream` delivers through the existing `accept()` queue and its 256-channel cap
  (`channel.rs:281-322`). `ChannelTransport` gains `TlsStream { medium }`; TCP keeps `TlsTcp`.
- `probe()` sends `n` timestamped lane-0 pings (9.5) before any application message and returns RTT
  p50/p95 and losses.
- **The host is always the TLS server**, with the same pins and identity on USB. A cold plug with
  no host selected uses `connect_secure_any_with(trusted_hosts, paths)`, which reports the matched
  pin so the client sends that host's resume reference.
- **Admission:** the same bounded set (16 pending, 5 s) and trust fence, plus ≤ 2 pending handshakes
  per USB device and a shared cap of 2 for loopback accepts; revocation closes USB channels like TCP
  ones (`channel.rs:259-279`). Interface-scoped paths keep NCM candidates off Wi-Fi. Pairing becomes
  generic over a byte stream, with the same OPAQUE contexts, certificates and pins.

### 8.3 Records and fragment size

Today each 16 KiB fragment becomes three TLS records (`multiplex.rs:541-579`). The sender writes
length prefix, 18-byte header and data in one call and sizes `CHUNK` so a lane record fits one
16,384-byte TLS record (16,362 data bytes). On AOA, a lane record of up to 16,324 data bytes
(16,384 − 16 link header − 22 TLS record overhead − 22 lane record overhead) also fits one
link-layer record in one 16 KiB accessory read; that is an efficiency choice only, since the link
layer carries a byte stream. A test asserts one TLS record per lane record.

### 8.4 Providers (`crates/streamcore-usb`)

Host providers implement `UsbPipeProvider { events() -> Stream<Attached | Detached>; open(device,
service) -> ByteDuplex }` (`service` is `Session` or `Pairing`) and hand each pipe to
`accept_stream`. Provider metadata (serials, UDIDs, identity strings) never feeds a trust decision.

- **adb** (Android): Rust speaks the host protocol on `127.0.0.1:5037` (`host:version`,
  `host:track-devices-l`, per pipe `host:transport:<serial>` + `tcp:<port>` or
  `localabstract:<name>`): adb forward, opened by the host, reverse as fallback; wireless-debugging
  serials ignored. Never kill a same-or-newer server, start one only when none runs, and never run
  a bundled client against a foreign server.
- **Device listeners** (adb, usbmux): Rust tokio listeners on 127.0.0.1, up to 4 pipes, with a fixed
  fallback port list the host also probes.
- **AOA** (Android; macOS via IOUSBHost over `ByteDuplex`, Linux via `nusb`; not Windows): a link
  layer over the bulk endpoints (16-byte header: magic, kind, flags, sequence, session, length;
  kinds `linkHello` (first frame, carries the link version), `open`, `opened`, `data`, `close`,
  `ping`/`pong`, `pad`). Any gap, duplicate or desync closes that session and its channel. Only the
  consumer a device is paired with may switch it; a cross-consumer lock stops two hosts fighting.
- **usbmux** (iOS/iPadOS): a Rust client for the header + plist protocol (`/var/run/usbmuxd`, or
  `127.0.0.1:27015` on Windows). **USB-C network interface** (Mac ↔ iPad/iPhone): an IP link with
  interface-scoped `UsbNet` candidates, QUIC off until qualified.

Matrix: macOS → iPad NCM (usbmux fallback), Android AOA (adb fallback); Windows → iPad usbmux,
Android adb; Linux → iPad usbmuxd, Android AOA + adb. Each consumer configures its own ports and AOA
identity strings; only one app holds an Android accessory at a time (one shared port: Q6).

### 8.5 Trust and pairing over USB

A cable is not authentication: device loopback ports are open to every app, ports 5037 and 27015 to
every local process, a "charger" can emulate adbd, and a hostile host can switch a phone into
accessory mode. Every medium runs the same pinned mutual TLS; locator labels never show as
identity; an unknown fingerprint fails closed and never prompts; pairing starts only from the
client's UI. An automatic pairing window (Q5) needs an unlocked host with a user present, opens at
most once per device per 10 minutes, shares one global failed-attempt budget, binds to the
triggering medium and shows the PIN only on the host. Negative tests: replug storm, fake adbd, fake
usbmux, squatted device port, malicious host over AOA.

### 8.6 Hot-plug, liveness and queueing

Host events come from IOKit, usbmux `Listen`, adb `track-devices-l`, `CM_Register_Notification` and
udev; client events from Android accessory detach and `USB_STATE`, and iOS `NWPathMonitor`. Over
usbmux and adb the client gets no detach event and a half-open relay may never deliver EOF, so:

- **Rust lane-0 liveness on USB:** a ping every 150 ms; dead after 3 missed pongs or 750 ms with no
  bytes. Rust reports each ping it answers to the consumer, which may derive state from it.
- **Relayed-media cap.** Relays queue in userspace, so the multiplexer caps unconsumed bytes across
  non-interactive lanes to about RTT × capacity + one fragment. Gate per medium: interactive p99
  under video plus a file transfer ≤ idle p99 + 2 ms.

### 8.7 Connect and switch (client)

No operation races a live transport.

**One transport owner.** The client has exactly one transport owner, a state machine that serialises
connect, reconnect, switch and failover: `Idle → Connecting → Live`, `Live → Switching → Live`,
`Live | Switching → Resuming → Live`. Only the owner sends a control hello, at most one at a time,
and no switch starts while `Connecting` or `Resuming`. If the live transport dies while `Switching`,
the owner enters `Resuming` and adopts the candidate as its first route if it passed its probe,
else cancels it.

**Compare-and-swap resume** (host defence in depth). Every control hello carries the generation the
client last saw. The host attaches with `SessionDaemon::attach_expecting(peer, resume, generation)`,
which fails with `SessionError::StaleGeneration` (consumers send `session.stale`) when the current
generation differs. A rejection leaves the current transport untouched and carries the current
generation, so a late control hello from an abandoned flow cannot supersede; if both of the owner's
channels died with the outcome unknown, it retries once with that generation. Plain `attach` keeps
last-wins.

**Connect or reconnect with no live transport.** The race stays in the consumer's dialer for v1; a
healthy USB pipe is one more route, a single `Pipe` path at delay 0 ahead of the network routes
(moving the race into Rust: Q11).

**USB trigger while on the network** (make-before-break): a physical plug, an accessory attach, a
pipe accepted on the device's loopback listener, the NCM interface settling, or a ready pipe that
lost the connect race. A pipe that arrives during the flap backoff (below) stays idle, still a route
if the network fails.

1. **Stability hold:** 2 s with no detach or USB-state flap before the TLS ClientHello (the host's
   60 s idle-pipe limit, 8.8, far exceeds it).
2. **Single-path dial:** exactly one `Pipe` path, pinned to the current host, no retry. On failure
   nothing else happens until the next trigger.
3. **Probe:** `probe(5, 20 ms)`. Pass: at least 4 pongs, RTT p50 ≤ the current transport's p50 + 2
   ms, done within 500 ms. On failure close the pipe cleanly and wait for the next trigger.
4. **Control hello** with the current resume reference and generation on the new channel.
5. **On the ack** (resumed, same reference, a newer generation, `continued` listing the moved
   streams): swap channel, receiver and keepalive in one step, report "transport changed" (not
   "interrupted"), close the old channel and ignore anything still arriving on it.
6. **On a rejection** close the new channel and keep the old one. **If 5 s pass with no answer**,
   keep both channels until one gets an ack (step 5) or a close; if both close, enter `Resuming`.

**Unplug or USB death.** The client enters "resuming", not "disconnected": the last frame stays on
screen and input is dropped. It races the pre-resolved network routes, sends the control hello with
resume and generation, and continues every stream in `continued`; others take the restart path.

**Path changes depend on the medium.** On a pipe medium (adb, usbmux, AOA) the client ignores OS
network-path events; liveness is the 150 ms lane-0 ping. On `UsbNet` or the network it acts only
when the local address or interface the current transport uses has gone: QUIC `rebind` first
(`channel.rs:201-217`), else make-before-break. Address rotation, DHCP renewal, VPN, or an unused
interface changing does nothing. Only the health rule or USB death moves a session off live USB.

**Health and flapping.** Health measures capacity, not consumption. The client moves to the network
(make-before-break) when, for 5 s, the USB sender is backlogged (`frameTiming`'s H5 → H6 above one
frame interval, or its backpressure flag, 9.4) while consumed throughput is below the target rate,
or the USB ping RTT p95 exceeds the network's last p95 + 2 ms. It then holds triggers for 10 s;
after 3 USB losses in 60 s it stays on the network for 5 minutes, doubling on repeat.

### 8.8 Pipe lifecycle and host handover

**Pipes.** The host provider owns them: one per attached device and service, opened when the device
appears, idle until the client's first TLS byte. The host keeps at most one idle pipe per device and
service, closes it after 60 s idle, and starts the 5 s handshake deadline at the first byte. A
closed pipe is reopened after 2 s while the device stays attached. Normal closes (idle timeout, a
clean client close after a failed probe or health move, a lost connect race) never count against
that; abnormal closes (I/O errors, handshake failures) count, at most 3 per attach, after which only
a new Attached event reopens; a pin failure stops pipes to that device until replug. After TLS and
the transport Hello, 10 s until the control hello, at most 2 connections waiting per peer
fingerprint, with lane-0 pings answered meanwhile so the probe fits.

**Host handover** (consumer code, with StreamCore support):

1. A per-session object owns the session's streams, the screen-holder identity and the audio
   subscription; the transport object becomes a sink only.
2. Screen authority and the input lease are keyed by session, so they survive a generation bump.
3. At attach with resume, whether the previous transport is live or parked, the host rebinds each
   stream's sink on that stream's executor (`attach_transport(sink)`), drops video not yet sent to
   the old sink, resets the post-encoder bounds, forces an IDR with the same stream ID and
   generation (a keyframe after a gap already decodes, 2.2), resends the audio config, releases held
   keys and buttons, and lists the moved streams in the control hello ack's `continued` field. The
   new sink then drops every frame submitted before the forced IDR, keyframes included: the up to 3
   frames still in the encoder at attach, which may include a pending recovery IDR, so the client
   sees exactly one keyframe and no delta after the gap triggers a keyframe request. A recovery in
   flight is satisfied by the forced IDR. These drops release their permits, are not losses (no
   `lost_seq`, no recovery) and are not counted as shed (6.4).
4. A superseded transport only closes its pipe.
5. **Park.** When a transport is lost, its session's streams stop admitting frames but keep their
   state for `T_park` = 3 s (`detach_transport()`); a resume within it rebinds them, after it they
   stop as today. A parked session is not current, so another device can still take the screen.
6. A session has at most one attached transport. The probing channel is pre-hello and owns nothing.

**Ownership.** StreamCore: `DialTarget::Pipe`, TLS over `ByteDuplex`, `accept_stream`, handshake
caps, no pipe retry, timestamped ping and `probe()`, USB liveness, `detach_transport` /
`attach_transport`, `SessionDaemon::attach_expecting` (`attach` is unchanged), and optionally the
transport owner as a pure state machine the consumer drives. Consumer: the control hello schema
(`resume`, the expected generation, the new `continued` field, a hard bump), the per-session object
and authority rekeying, make-before-break, UI states, provider wiring.

### 8.9 Failover budget and tests

Measured from the physical event, with the 2.4 baseline:

| Step | Budget |
| --- | --- |
| Plug switch: host attach → first IDR decoded on USB | ≤ 150 ms; no stream restart visible |
| Unplug detection | ≤ 750 ms; about 150 ms with a client event |
| Network dial (pre-resolved routes) | ≤ 150 ms on a LAN |
| Control hello with resume | ≤ 50 ms (VM, 2.4: median 10–24 ms, max 30 ms) |
| Forced IDR encode + delivery | ≤ 100 ms |
| **Unplug → first decoded network frame, stream continued** | **≤ 1.05 s worst case** |
| Restart path only (park expired, stream not continued): old capture stop + capture start + first IDR | measured in P0/P3 (VM soak, 2.4: video decoding again p95 272 ms, max 383 ms after resume) |

A target with a client event (proposed ≤ 500 ms) is set only after measuring on hardware. Plug → USB
takeover is ≤ 2 s after the pipe or interface is ready, plus the stability hold. At a switch
(credits reset) in-flight lane-0 commands have unknown outcome and held inputs are released; video
and audio continue; terminal panes and bulk transfers resume through their verified paths.

Tests (P3 gate):

- a plug while healthy: exactly 1 attach, the same stream ID, exactly 1 keyframe (with 3 frames in
  the encoder at attach, a pending recovery IDR among them, none is sent before the forced IDR), 0
  client keyframe requests, 0 interruptions, the input lease kept; a failed probe or pipe TLS gives 0
  attaches and leaves the network untouched;
- a healthy USB link with a static or low-rate stream stays on USB for 10 minutes; Wi-Fi address and
  interface changes while streaming over USB give 0 switches and no teardown on any medium;
- the network dies mid-switch: exactly one attach, no supersede of the adopted transport; a control
  hello delayed past another flow's ack is rejected as stale; a step-6 timeout whose attach did
  happen ends on the new channel;
- a pipe killed after the control hello resumes over the network within `T_park`, stream continued;
  park expiry takes the restart path; a pipe path is never retried; normal closes leave the reopen
  budget untouched;
- the pre-hello deadline closes an idle connection while its pings are answered; handover
  interleaved with stream start and stop ignores completions from the old sink.

## 9. Measurement

### 9.1 Definitions

**Glass-to-glass (G2G)**, by camera or photodiode, is the authority for latency gates. **Automated
G2G** is (H0 → C5) + (client − host panel term, each measured once per device), used in CI. **Click
round trip** is today's pacing metric, kept for regressions.

### 9.2 Trace points

Keyed by `(stream, generation, sequence)`; host and client values each on their own monotonic clock.

| Id | Point | macOS / Windows / Linux / client |
| --- | --- | --- |
| H0 | source present | `displayTime` / `SystemRelativeTime` or `LastPresentTime` / `spa_meta_header.pts` |
| H1 | capture callback | SCK handler / `FrameArrived` / `process` |
| H2 | convert/compose GPU-complete | Metal completion (none for NV12) / D3D11 timestamp query or fence / VA sync |
| H3 | encoder input accepted | `EncodeFrame` return / `ProcessInput`, `nvEncEncodePicture` / `vaEndPicture` |
| H4 | access unit available | encoder output callback |
| H5 | `VideoSink` admit | Rust, on the executor |
| H6 | last fragment written | multiplexer |
| C0–C5 | first byte, message complete, decode submit, decoded, present request, presented | Rust receiver; VT output handler / `onOutputBufferAvailable`; `addPresentedHandler` / `setOnFrameRenderedListener` (calibrated) |

### 9.3 Budget

2560×1440, 60 fps source, p50 / p95 in ms, estimates. The end-to-end figure is **derived from P0's
measured per-stage distributions** (sampled jointly per frame, or measured directly as H0 → C5),
never by adding stage p95s.

| Stage | Budget p50 / p95 | Notes |
| --- | --- | --- |
| H0 → H1 capture | 2 / 8 at 60 Hz; 2 / 4 at 120 Hz+ | measured in P0 before any gate uses it |
| H1 → H2 convert/compose | 1 / 2 | NV12 single target 0; Metal canvas 0.5–1.5; D3D11 1–2.5 |
| H2 → H3 wait for a permit or `NeedInput` | 0 / 2 | new row; nonzero means admission is the bottleneck |
| H3 → H4 encode, one in flight | 4 / 8 | VT, NVENC 2–8; Intel Gen9-class H.264 5–10 |
| H4 → H6 hop, admit, packetize, encrypt, write | 0.5 / 2 | includes the executor hop (3.5) |
| cross-adapter copy (hybrid only) | TBD | measured in P5 (4.2) |
| **host H0 → H6** | **≤ 8 / ≤ 14** same adapter; **≤ 12 / ≤ 20** hybrid or Intel Gen9-class | |
| network incl. serialization | wired 1 / 2; 5 GHz Wi-Fi ≈ 4 / 12; USB relays (adb, usbmux, AOA) and NCM measured in P3a/P3b, not assumed faster than wired | Wi-Fi includes airtime contention; the VM probe saw QUIC video RTT p95 14–26 under bulk |
| C0 → C1 receive, decrypt, reassemble | 0.5 / 1 | |
| C1 → C2 hand-off from Rust to the native decoder | 0.2 / 1 | new row; includes the sample copy and executor hop |
| C2 → C3 decode | Apple 3 / 5; Android 6 / 10 | Apple measurable from P2 |
| C3 → C5 vsync + compositor (Responsive) | 8 / 17 at 120 Hz; 16 / 33 at 60 Hz | more when GPU-composited |
| panel term (client − host) | per device pair | camera rig |
| **G2G** | proposed gate p95 < 50 on a **wired or USB** link with a 120 Hz client (Q8) | Wi-Fi and 60 Hz reported, not gated |
| recovery | loss → first good frame ≤ 2·RTT + 1 frame (LTR), ≤ 100 (IDR, LAN) | loss → text-sharp also reported |
| failover | 8.9 | |

### 9.4 frames.v4 and the lane-0 video kinds

frames.v4 is a transport Hello capability. The 29-byte v3 header is unchanged; flag bit `0x80`
("extension follows", v4 only) adds `ext_len u16 LE` and TLVs `[type u8][len u8][value]`: `0x01
timing` (u32 µs offsets from H0 for H2, H3, H4, plus i32 H1 − H0), `0x02 capture_us` (u64 H0), `0x03
discardable`, `0x04 ltr` (u32 token + u8 flags), `0x05 temporal_id` (u8) and `0x06 content` (u32
`layout_epoch`).

**PTS.** `capture_us` always keeps the frame's H0, including on recovery re-encodes. `pts_micros`
stays the strictly increasing submission time: recovery frames are re-stamped with the current time,
as the reference host does.

**Lane-0 video kinds.** `videoFeedback`, `videoNack`, `frameTiming` and `layoutInfo` become
StreamCore binary kinds on lane 0, defined in `protocol/BINARY.md` like the typed ping and pong, with
fixtures in `crates/streamcore-media/tests/binary_contract.rs`; proposed kinds `0x12`–`0x15` (fixed
in P2). The default lane rule
(`multiplex.rs:63-68`) would put them on lane 0, but today `Frame::valid_kind`
(`crates/streamcore-media/src/binary.rs:399-401`) rejects them in `Frame::read` (`:370`) and
`Frame::encode` (`:390`), so the host can neither encode them nor route them: `application_lane`
(`multiplex.rs:53-56`) refuses them before any lane rule runs. Each consumer's native frame codec
has the same allow-list. `valid_kind` is stateless and cannot see negotiated capabilities, so P2
widens it unconditionally and senders emit the new kinds only when frames.v4 was negotiated (10):

- `videoFeedback` (client → host, about every 100 ms; every 20 ms in datagram mode): highest
  complete sequence, optional loss bitmap, consumed bytes, per-frame receive/decode/present times,
  flushes, present drops, optional LTR ack.
- `videoNack` (datagram mode only): missing fragments of a frame.
- `frameTiming` (host → client, sampled; every frame on USB): H5 and H6, which cannot be in the
  frame's own header, and the sink's backpressure flag (8.7 health rule).
- `layoutInfo`: epoch, source rect, content rect, rotation (3.6).

Only `MediaCaps` and the stream-started `features` stay in the consumer's control protocol. Unknown
TLVs are ignored; `timing` is sampled (every frame in qualification). `StreamController.stats()`
returns per-stage percentiles, the (a)–(e) skip counters, the shed rate, encoder, adapter,
transport, medium, RTT, loss and decoder for consumer diagnostics.

### 9.5 Clock model

Today's ping and pong are a consumer-handled echo with no host timestamp. P0 defines a typed body in
`protocol/BINARY.md`: the ping carries the client's send time and a nonce, the pong the nonce and the
host receive and send times (µs). Rust answers on lane 0, before or after any application message
(also serving USB liveness and `probe()`), estimates the offset NTP-style from the minimum-RTT
sample of a 10 s window and tracks drift; one-way numbers carry ±RTT/2. Ships as a pair (D13).

### 9.6 Qualification

- **Frame-counter barcode** instead of the four-colour cycle: every source frame is counted as
  presented, repeated or skipped, and each skip is attributed through the (a)–(e) counters.
- **Unloaded physical baseline.** P0 records click → decoded, click → presented and per-stage
  p50/p95/p99 over at least 10 runs on unloaded physical hardware, with 95% confidence bounds; a run
  above the load threshold is repeated, not counted.
- **Pacing gates** (60 fps source, 60 Hz client): skipped ≤ 1%, repeated ≤ 1%, presented interval
  p95 ≤ 20 ms and p99 ≤ 33 ms, no gap > 100 ms except a logged recovery; delta shed rate ≤ 0.1% on
  wired links (reported on Wi-Fi).
- **R1/R2 checks:** every frame's H4 → H5 and C3 → C4 gaps stay well below one frame interval at 30,
  44 and 60 fps; a still-screen test shows the last change at once.
- **Matrix:** 512 px and native; H.264 and HEVC; QUIC, TLS/TCP and each USB medium; with and without
  bulk (including a real file transfer); 0, 0.5 and 2% loss with 5–20 ms jitter.
- **USB** per medium: idle and loaded RTT, throughput, IDR delivery time, plug and unplug timings
  from the physical event, 100 plug cycles with nothing leaked, wrong pins, OS prompt flows.
- **GPU (Windows):** MF contract test per encoder, a GPU-loaded run, the VUI test. **Colour:** the
  0.12 RGB gate on every encoder and compositor.

### 9.7 CI and hardware

- **CI (D14).** No GitHub self-hosted runners, ever: on a public repository collaborator branches,
  `workflow_dispatch`, approved fork runs and `pull_request_target` can reach one, and a build
  machine may hold signing identities. While private, CI is `ci/local/jobs.json`; once public,
  GitHub-hosted runners run the Rust and contract jobs only. VM, device, GPU and USB qualification
  runs outside GitHub Actions against a pinned tag; reports record hardware class, adapter and
  encoder tier.
- **Reference hardware classes:** an Apple-silicon Mac host; a 120 Hz Apple client; a mid-range
  Android tablet; a flagship-class Android device; an Android device that restricts USB debugging;
  an Intel Gen9-class iGPU Windows host; a hybrid NVIDIA laptop; an AMD host; a Linux host; a Windows
  VM for protocol and CPU-fallback runs only. A gate naming a class closes only on it (Q7).

## 10. Contract changes

The session ALPN (legacy value `mycnc/2`), the transport major and the legacy constants do not
change. Each bump below is a hard capability break: host and client ship together.

| Change | Mechanism | Owner |
| --- | --- | --- |
| timing, capture time, discardable, LTR, temporal id, layout epoch | transport Hello capability `frames.v4` | StreamCore |
| `videoFeedback`, `videoNack`, `frameTiming`, `layoutInfo` | lane-0 binary kinds in `protocol/BINARY.md`, under frames.v4 | StreamCore |
| accept kinds `0x12`–`0x15` in the frame codec (read and encode) | `Frame::valid_kind` (`binary.rs:399-401`), widened unconditionally, and the envelope list (`protocol/BINARY.md:120`); senders emit them only under frames.v4 | StreamCore |
| the same kinds in each consumer's native frame codec allow-list | consumer codec change, shipped with frames.v4 | consumer |
| `stream.*` wire codes (3.7) | a `protocol/` file next to `BINARY.md` | StreamCore |
| typed ping/pong, handled in Rust | `protocol/BINARY.md` + fixtures | StreamCore |
| `MediaCaps`, stream-started `features`, control hello expected generation and `continued`, `session.stale` | consumer control protocol bump (each consumer's own) carrying StreamCore-defined records | consumer |
| `SessionDaemon::attach_expecting`, `SessionError::StaleGeneration` | API only; `attach` unchanged | StreamCore |
| single-write lane records, per-transport `CHUNK` | sender only | StreamCore |
| USB: `TlsStream`, `PeerLocator`, `DialTarget::Pipe`, `accept_stream`, `probe`, `connect_secure_any_with`, generic pairing stream | API only; same ALPN, transport Hello, records, resume, pairing and pins | StreamCore |
| datagram video | capability `video.datagram.v1`, QUIC only, new LANES.md section | StreamCore |
| "encoded bytes cross only through the send and receive calls" (3.2) | doc comments `stream.rs:1-2`, `media.rs:1-2` | StreamCore |

## 11. Phased plan

Each phase starts from a StreamCore tag; consumers move their submodule to it.

| Phase | Scope | Exit gate |
| --- | --- | --- |
| **P0 Measure** | citation re-check; the `stream.rs`/`media.rs` rewording (3.2); H0–H6 on today's macOS path; C0–C2 and C2 → C5 on today's Apple client; typed ping/pong in Rust and clock sync; barcode test; skip counters; experiment E1 | per-stage p50/p95 in reports; every skip attributed; R1/R2 checks reported; the unloaded physical baseline (9.6) with confidence bounds; no video behaviour change |
| **P1 Traits + executor + core port** | `streamcore-video` with the executor, reentrancy rule, close and observer model, and traits; `StreamController` ports (lifecycle, recovery, latest frame, sizing, command layer, canvas policy) with the Swift tests first; SCK/VT adapters; view/control mode; Rust as single validator; UniFFI async foreign-trait test. Video leaves through the `VideoOut` hop to the consumer's existing send path (3.5) | the 3.5 tests; every existing signed gate; pacing and latency within the P0 confidence bounds; FFI overhead p99 ≤ 50 µs per frame |
| **P2 Pacing, send path, compositor** | drop-before-encode; adaptive permits; `VideoSink` **with the bulk token bucket in the same release**; `TCP_NOTSENT_LOWAT`; QUIC tuning; immediate first recovery; temporal layers and LTR on VT H.264; delta age bound only where confirmed; ABR; frames.v4, lane-0 kinds (with the `valid_kind` and consumer codec changes) and the consumer control bump; Metal compositor; Apple client on `VTDecompressionSession` | skipped ≤ 1% and interval p95 ≤ 20 ms on an unloaded physical Mac → iPad link, wired or USB (Q8; Wi-Fi reported); scheduling-probe gap p95 ≤ 1.5 × frame interval under bulk including a real transfer; canvas and colour gates; R1/R2; shed-rate gate |
| **P3a USB, Mac ↔ iPad** | `TlsStream`, `Pipe` paths without retry, `accept_stream`, `probe`, single-write records, dual-stack sockets (CI on Windows), handshake caps, generic pairing; NCM and usbmux providers; Rust loopback listeners and liveness; the transport owner, `attach_expecting`, make-before-break, park and handover (8.7–8.8) | the 8.9 tests and budget; wrong pin fails on every medium; pairing over USB; interactive p99 ≤ idle p99 + 2 ms under load; terminal continuity across an unplug; 100 clean plug cycles; negative tests |
| **P3b USB, Android transport** | first run of the Kotlin bindings; Android builds; AOA (with sequence numbers and host arbitration) and adb forward (reverse fallback); a harness app without a decoder; adb and usbmux providers on a Windows host | P3a gates on Android; forward vs reverse measured; AOA and adb negative tests; Windows provider runs |
| **P4 Android client** | async MediaCodec `Decoder`, vendor keys, zero-reorder SPS, SurfaceView refresh policy | R2 trace check; C5 calibrated; decode C2 → C3 p95 ≤ 10 ms on a flagship-class Android device (Q7) |
| **P5 Windows host** | `streamcore-video-windows` (4.2): WGC, adapter policy, hybrid spike, shader convert, MF MFT, software fallback, NVENC, VUI normaliser, Windows diagnostic runner | host H0 → H6 p95: H.264 ≤ 14 ms on an Intel Gen9-class iGPU at 1440p60; ≤ 8 ms on NVIDIA same-adapter (Q7); MF contract and VUI tests; GPU-loaded run; pacing, R1, and the G2G gate on a wired or USB link (Q8; Wi-Fi reported) |
| **P6 Apple measured renderer** | Metal + `CAMetalDisplayLink` A/B | pacing suite decides; colour gate |
| **P7 Linux host** | 4.3, AOA via `nusb`, usbmuxd | P5 host gates on Q7 hardware |
| **P8 Datagrams** (conditional) | 7.3 | G-DG |

USB lands on macOS first because macOS exercises every medium except Windows usbmux, which P3b
covers. Consumer migrations follow their own plans.

### 11.1 Rollback

| Phase | How to back out |
| --- | --- |
| P0 | instrumentation is off by default; the typed ping/pong is a hard break, so revert by shipping the previous host and client builds together |
| P1 | the reference host's Swift pipeline stays selectable at runtime (a host setting) until P2 passes on hardware; switching back needs no client change |
| P2 | `VideoSink` bounds, the delta age bound, the bulk bucket and ABR each sit behind a flag; frames.v4, the lane-0 kinds and the consumer control bump revert only by shipping the previous host and client together |
| P3a/P3b | each USB provider sits behind a host kill switch; handover and park sit behind a flag whose off state is today's restart path |
| P4–P7 | new backends are selectable; the previous backend or build stays available until the phase gate passes |
| P8 | datagrams are a negotiated capability; stop offering it |

## 12. Risks

Sections carry their own mitigations. Without a home section: licensing (AGPL-compatible
dependencies, codec patents) and the undocumented usbmux protocol; mitigated by permissive crates,
OS codecs, legal review, and NCM as the primary Mac ↔ iPad medium.

## 13. Open questions

1. Unsafe lint set for the platform crates and `streamcore-usb` (3.1).
2. Windows host shape: one Rust host, or a consumer shell talking to a Rust host over local IPC?
3. If one QUIC stream per frame ever beats datagrams (P8), is it a capability or a new major?
4. Is a Windows secure-desktop helper needed, and for which consumer?
5. May plugging a cable open the host's pairing window automatically under the 8.5 limits?
6. One port with capability dispatch once both consumers run on StreamCore?
7. Which reference hardware gates P4, P5 and P7 (9.7)?
8. Gate definition: p95 G2G < 50 ms on a wired or USB link with a 120 Hz client, camera as
   authority, automated G2G in CI, Wi-Fi and 60 Hz reported only?
9. Are `T_park` = 3 s, the 10 s pre-hello deadline, the 600 ms resize settle and the 500 ms
   `invalidate` bound right?
10. Default present mode per consumer; cursor metadata later?
11. Should the connect race and incremental resolution move into Rust, and when?
12. Should adapters every consumer writes identically ship as StreamCore packages?
