# Video stream threading and reentrancy rules

Normative for `crates/streamcore-video` and every backend that implements its traits (Swift,
Kotlin and Rust alike). It restates section 3.5 of [STREAMCORE-VIDEO.md](STREAMCORE-VIDEO.md) as
implemented in P1 and says how each rule is enforced and tested.

## The executor

- Every stream has exactly one executor: a FIFO mailbox plus a timer set, drained by one thread at
  a time on the stream's `SerialQueue`. The queue calls `StreamPort::run()` for a wake and
  `StreamPort::run_timer(token)` when a `wake_after` deadline passes.
- The executor owns all stream state: the lifecycle phase and pending operation, the latest slot,
  the encoder slot and its generation, the in-flight table, the packetizer, the `submitted_seq` /
  `delivered_keyframe` / `lost_seq` fences, recovery state and timers. Nothing else reads or
  writes it. Other threads see only the snapshot it publishes (`phase()`, `stats()`).
- A drain takes the events queued when it starts and handles them in order. Anything posted while
  it runs, including jobs the executor schedules for itself, waits for the next wake.
- The executor never blocks: no sleeps, no `.await`, no synchronous waits, with two bounded
  exceptions:
  - `Encoder::invalidate`, bounded by `StreamTimings::invalidate_bound_ns` (500 ms). An adapter
    whose platform call has no timeout runs it off the executor and waits with one (`run_bounded`).
    When the call overruns the bound, the executor posts `ObserverJob::EncoderFailed`, counts it
    (`invalidate_timeouts`) and releases the hung session's permits, so a hung flush cannot stall
    admission; a re-create continues with the new session. The hung session's later outputs carry
    the old generation and are discarded.
  - `EncoderFactory::create` (MFT activation or an NVENC open can take over a second). The executor
    runs it on a helper thread and waits at most `StreamTimings::create_bound_ns` (3 s). On overrun
    the start or re-create fails with `stream.encoder` (`create_timeouts`), and the late session is
    invalidated and dropped on the helper thread.
- The executor never runs on a `streamcore-network` worker.

## Inbound calls only post

`CaptureEvents::*`, `EncoderEvents::*`, `SinkReceipt::complete`, their C ABI forms (the
per-frame fast path below), permit releases and every `StreamController` method except
`close_and_wait` enqueue an event and return in O(1). They touch
no stream state and call no native code except `SerialQueue::wake`, which `post` calls after it
has dropped the mailbox lock.

**Inline drain, frames only.** When `CaptureEvents::frame` (or its C form) arrives while no drain is active and
nothing is queued ahead of it, only admission, convert and submit for that frame run inline, so a
capture callback reaches `submit` with no hop. Everything else that frame causes (an encoder
re-create, a failure, an observer post) is posted for the next wake, so nothing calls back into
the capture backend from its own callback. A newer frame for the same capture and member replaces
an undelivered one in the mailbox (its lease is released).

**Inline drains run on the caller's thread.** Backends deliver capture callbacks on the stream's
queue, and only then does an inline drain stay on that queue. Nothing checks this. A backend that
delivers frames on other threads (a free-threaded WGC frame pool) is still correct, because the
drain flag keeps drains exclusive, but its convert and submit then run on those threads: its device
must be multithread-protected, and thread-affine objects (a `DispatcherQueueTimer`) cannot be made
there.

**Capture calls run on the queue's thread.** `CaptureBackend` `caps`, `open`, `start`, `update` and
`stop` run only in drains the queue started (`StreamPort::run`, `run_timer`), never in an inline
frame drain, so always on the queue's own thread. A backend that must create thread-affine objects
there (a WGC frame pool bound to the calling thread's `DispatcherQueue`) does so in `open` or
`start`. Tested in `trait_fit.rs`.

**Capture-side skips.** A backend that drops frames unseen (it kept only the newest of a pool
backlog) reports them with `CaptureEvents::skipped(id, member, count)`, a leaf call that counts
skip cause (a) into `StatsSnapshot::capture_skipped` and posts nothing.

**Stale events.** Every event carries its identity: encoder generation, capture id, op token or
timer token. A stale event is discarded after its permit and lease are released.

## Outbound calls run only on the executor, during a drain

`CaptureBackend` open, start, update and stop; `Converter::convert`; `EncoderFactory::create`
(issued by the executor, run on its bounded helper thread);
`Encoder` submit, `set_rate` and `invalidate`; `VideoOut::send` and `VideoOut::backpressured`;
`ObserverQueue::post`; `SerialQueue::wake_after`. Results of synchronous calls are values
(`SubmitResult`, `Result`), never callbacks. Asynchronous results (`op_done`, `encoded`,
`dropped`, receipts) are always posted, even when known at once.

`convert` and `submit` have exactly one caller, the executor's internal submit job, scheduled by a
frame, a permit release, an output, a drop, `EncoderEvents::input_ready` or a recovery timer,
never by a public call. After a `Busy` submit the frame stays retained; the encoder calls
`input_ready` once it can take input (MF `METransformNeedInput`), so a still screen still gets its
frame encoded.

## Leaf calls

These may run on any thread. Each uses atomics or at most one short, non-nested Rust lock that is
released before any native call, and none waits on an executor:

- `post` (the mailbox lock, then `SerialQueue::wake` with the lock dropped);
- `VideoWorkPermit::release` and its Drop (atomics; then the release hook posts
  `PermitReleased`, or, on the owning executor's own drain, flags it for that drain);
- `VideoWorkQueue::close` (an atomic);
- `VideoSinkHandle::submit_encoded` (the handle's lock) and the C ABI's
  `streamcore_video_encoded` (the sink registry's lock, then the handle's, then `post`), and the C
  ABI's packet completion and release (the packet registry's lock, then `post`);
- every backend's `release(surface)`: thread-safe, non-blocking; Rust calls it when a lease drops,
  on the executor while it exists, else on the thread that drops it;
- `VideoOut::revoke` and `VideoOut::backpressured`;
- reads of the published snapshot.

`SerialQueue::wake` and `wake_after` never block and take no app lock.

## Permits

`VideoWorkPermit` is an atomic `released` flag over one atomic count that also carries the closed
bit: at most 3 permits per stream, counted across encoder replacement, and a close racing an
acquire never admits after it returns. Every release posts `PermitReleased` to the owning
executor, which then submits the retained frame if one is waiting: on a still screen nothing else
would retry it. A release on the executor's own drain (the executor finishing a sent frame) is
handled by that drain before it ends instead of being posted, which would cost a wake and a second
drain per frame; one inside an inline frame drain is still posted.

The executor acquires a permit before `submit` and keeps a reference in its in-flight table. It
releases it explicitly after `VideoOut::send` returns for that frame's packets, or when the frame
is dropped or stale. A native encoder keeps its own reference until it has posted the frame's
`encoded` or `dropped`, then drops it; on `Busy` or `Failed` it drops it before returning. When a
session is torn down, the executor drops (but does not release) its references, so a permit that
native code still holds keeps counting. The exception is an `invalidate` that overran its bound:
that session's permits are released (see above).

## Reentrancy rule

1. **No re-entry from a callback.** Code running inside an outbound call, or in a native callback
   triggered by one, must not call back into the same stream synchronously except through `post`
   and the leaf calls. A completion is always posted, even when it is known at once. (Enforced:
   every entry point posts; a frame that arrives during a drain is posted instead of drained
   inline, because the drain flag is taken.)
2. **No lock held across the boundary, in either direction.** Native code holds no lock (`NSLock`,
   `Mutex`, `os_unfair_lock`, `synchronized`, an authority block) while it calls into Rust, except
   around the leaf calls above. Rust holds no `MutexGuard`, including the executor's own, while it
   calls native code. (Enforced: the stream state is moved out of its mutex for the length of a
   drain; the mailbox, queue, sink and close locks are released before any native call.)
3. **No synchronous wait on an executor** from any executor or `streamcore-network` worker, or from
   anything an executor may wait on: callbacks, encoder or decoder output threads, or any native
   method reached from an outbound call. `Encoder::invalidate` waits on an encoder, not an executor;
   it is the one exempt wait.
4. **Encoder and decoder output threads touch no stream state.** They may copy bytes, do pure
   conversions (Annex-B to AVCC, avcC/hvcC), call `VideoSinkHandle::submit_encoded` (or the C
   ABI's `streamcore_video_encoded`) and post.
5. **Permits** are acquired on the executor before `submit` and released when the sink accepted the
   frame, or by Drop wherever native code drops them; every release posts `PermitReleased`.
6. **Debug builds assert** that every outbound call runs inside a drain of its own executor (a
   per-executor thread-local marker set by every drain, inline ones included), and that a blocking
   entry point (`close_and_wait`) runs on no executor and no `streamcore-network` worker (identified
   by thread name). In release builds `close_and_wait` returns `MediaError::Internal` there instead.

## Dropping native objects

The executor drops encoders at teardown, and every backend, lease and receipt at `Closed`, during
a drain. **Dropping a backend object must not block**: the final release of an MFT, a DXGI device
manager or a device after a reset can wait, so hand it to a helper thread. Surface releases are leaf
calls (above).

## Outputs, fences and layout

- Encoder outputs are length-prefixed: `BitstreamForm::Avcc4` samples and avcC/hvcC config. An
  encoder that emits Annex-B converts on its output thread (`bitstream::annexb_to_length_prefixed`,
  a pure function, allowed by rule 4); an Annex-B output fails the stream with `stream.encode`.
- `SurfaceInfo.fence` is informational when one native backend owns capture, convert and encode
  on one device: ordering is implicit on that device, and the backend keeps fences in its own slot
  table. Rust never forwards a fence to `submit`. A fence handle of `0`
  (`D3d11Fence { shared_handle: 0, .. }`, `MtlSharedEvent { handle: 0, .. }`) names the
  producer's own in-process fence on the stream's device: no shared handle exists and nobody closes
  one. A non-zero handle belongs to the producer and stays valid while the surface is leased.
- **Leases on the convert path.** The executor's hold on a capture surface for a submitted frame
  ends when `convert` returns: the encoder reads the converted surface, so a capture pool buffer is
  pinned only by the latest slot, not until the encoder's output. A converter whose GPU work still
  reads the input keeps it valid until its fence completes (a retained `CVPixelBuffer`, or a slot
  table shared with the capture backend that defers a WGC frame's `Close` to the fence). On the
  identity path the encoder reads the capture surface, which stays leased until the output.
- **Region crops.** For a `Region` target whose capture holds the whole display
  (`CaptureCaps::region_in_surface` false), each `ConvertJob::source` is the region in the input's
  pixels (even origin and size), following live region moves. Such a region needs
  `ConvertPath::Convert`; with `Identity` the start is refused (`stream.capture`). A backend that
  captures only the region (macOS `sourceRect`) reports true and gets `source: None`.
- **Stage times.** `VideoOutMeta::timing` carries the frame's H0 and H1 (from its `SurfaceInfo`)
  and H3 and H4 (from `EncodedFrame::timings`), so a consumer's send hop can trace a frame without
  a table of its own; a recovery re-encode keeps the image's H0 and H1.
- `CaptureEvents::layout_changed` (a display mode, DPI, rotation or window size change) posts
  `ObserverJob::LayoutChanged { epoch, layout }`; the consumer re-plans, for example with a size
  update. The executor does not resize by itself.

## Clocks

Every `*_ns` value in a record is host-monotonic nanoseconds on one clock per host, converted
natively:

- **Windows:** QueryPerformanceCounter ticks × 10⁹ / QueryPerformanceFrequency. WGC's
  `SystemRelativeTime` is already in 100 ns units of that clock: multiply by 100.
  Until WGC's `SystemRelativeTime` is checked on a native display (it was later than the
  FrameArrived stamp in a remote-desktop session), Windows stages start at H1.
- **macOS:** `mach_absolute_time` × timebase (the clock of `SCStreamFrameInfo.displayTime` and the
  host `CMClock`). It stops during sleep; `mach_continuous_time` does not, and would not match
  ScreenCaptureKit's or VideoToolbox's timestamps.
- **Linux:** `CLOCK_MONOTONIC`.

**Windows timer period (host rule).** The Windows backend holds a 1 ms timer period
(`timeBeginPeriod(1)` plus the timer-resolution power-throttling opt-out) from capture start
to stop, reference-counted across streams and never while idle. Quick Sync's encoder waits on
the system timer: about 15 ms per encode at the default 15.6 ms tick, about 6 ms at 1 ms. See
`crates/streamcore-video-windows/README.md`.

The executor's own `Clock` (`MonotonicClock`, measured from the stream's creation) drives timers
only. Timestamps the executor writes (a recovery frame's submission time) are derived from the last
frame's host time plus elapsed executor time, so they stay on the host clock.

## Authority and the P1 send hop

Screen authority is a revocation token held by the sink. In P1 the sink is the consumer's existing
send path behind `VideoOut`: `send` takes the adapter's sink lock (which `VideoOut::revoke` also
takes), checks the token and calls the consumer's send, so **once `revoke()` returns, no frame for
that authority is enqueued.** `StreamController::revoke` and `begin_close` set the stream's own
revoked flag and call `VideoOut::revoke`; the executor checks the flag before every submit, send,
start, update and encoder creation. Cleanup calls (stop, invalidate, release) still run. Every send
completion, a synchronous refusal included, comes back through the receipt; a receipt dropped
uncompleted counts as refused. Only completions the executor acts on are posted: a refusal (a loss)
and an accepted keyframe (the end of a recovery); an accepted delta or configuration changes
nothing and posts nothing. From P2, `VideoSink` replaces `VideoOut`.

## Close

- `begin_close()` revokes the sink, posts `Close` and returns at once; the async `close()` (Kotlin
  `closeStream()`, since UniFFI's Kotlin objects already use `close()` to free themselves) does the
  same and then waits without blocking. The completion is an `ObserverJob::Closed` on the observer queue; the async `close()`
  resolves at the same time.
- `close_and_wait(timeout)` is only for synchronous native teardown (rule 6). It returns `Closed`,
  or `StopUnknown` when the stop failed, did not answer within the op timeout, or the wait timed
  out. Tearing down several streams posts every close first, then waits.
- On `Close` the executor stops admitting (the work queue is closed), clears the latest slot,
  invalidates the encoder and stops the capture; it keeps draining in release-only mode. Once the
  stop's `op_done` arrived (or the op timed out) it enters `Closed`: it posts `Closed`, drops its
  references to the backends, encoder, sink and queue, and completes the close. Event ports hold
  the executor weakly, so this ends every reference cycle. A post to a port whose executor is gone
  releases any lease or permit it carries on the calling thread and drops the event. After the close
  completes, no outbound call is in progress and none follows.
- Dropping a `StreamController` without closing posts a close; the executor keeps itself alive
  until it reaches `Closed`.

## Observers

Consumer observers run only as jobs posted to the consumer's `ObserverQueue`, never on the
executor, so an observer may call anything, including `close_and_wait`.

## Tests

`crates/streamcore-video/tests/`: `single_queue.rs` ports the reference host's single-queue
pipeline tests onto mock traits; `executor_rules.rs` covers the inline drain, the deferred
re-create, the bounded invalidate, close, late callbacks and reentrancy; `lifecycle.rs` the
operation state machine; `stress.rs` seeded shuffles and many-thread runs; `native.rs` the
per-frame C ABI (layouts, a stream whose backends make every per-frame call through it mixed with
UniFFI calls, admission, busy and failed submits, refused bytes, dead keys, compact frames, one
wake per delivered frame). `StreamCoreVideo/` runs the Swift binding smoke test through the
UniFFI calls and through the C ABI.

## The per-frame fast path

Through UniFFI every call clones a handle, every record, enum, `Option` and byte string is
serialized into a buffer that the other side parses and frees with further calls, and a foreign
callback looks its object up in a locked map. Between frames at 120 fps the caches are cold, and a
frame's calls then cost a Swift backend tens of microseconds (design 3.5 "Overhead"). A foreign
backend therefore makes the per-frame calls through a small C ABI with primitives only
(`crates/streamcore-video/include/streamcore_video_native.h`, `src/native.rs`) and keeps UniFFI for
everything else. Rust backends call the traits directly.

| Per frame | C ABI | UniFFI form |
| --- | --- | --- |
| capture callback | `streamcore_video_frame(port, capture, surface, capture_ns, source_present_ns)`: the shape (kind, format, size, colour, rotation) repeats the last `Complete` `frame` of that capture, which a backend sends whenever the shape changes; not taken (false) before one, as after a restart | `CaptureEvents::frame` |
| submit | the attached `submit(encoder, surface, submit_seq, pts_ns, capture_ns, force_keyframe)`, returning submitted, busy or failed with an error code; the stream keeps the frame's permit until its output or drop is posted (by either ABI) | `StreamEncoder::submit` |
| surface release | the attached `release(capture, surface)` | `CaptureBackend::release` |
| encoder output | `streamcore_video_encoded(sink, &output)`: payload and configuration copied once, straight from the encoder's buffer, plus metadata; posts `dropped` itself when the bytes are refused | `submit_encoded` + `EncoderEvents::encoded` |
| wake, drain | the attached `wake(queue)`; `streamcore_video_run(port)` | `SerialQueue::wake`, `StreamPort::run` |
| send | the attached `send(out, &packet)`: metadata and the bytes' address and length, lent until `streamcore_video_packet_release(key)`, so the bytes are not copied out | `VideoOut::send` |
| send completion | `streamcore_video_packet_complete(key, accepted)`, before or after the release (a consumer may copy the bytes at once and complete later); the receipt is kept until completed or the stream closes | `SinkReceipt::complete` |

Native code names Rust objects by key (`StreamPort::native_key`, `VideoSinkHandle::native_key`, the
packet's `key`), never by pointer, and a key whose object is gone names nothing. The attached
functions get their context back unretained: it must be the backend object the stream was given
through UniFFI (the queue, capture backend and `VideoOut` of `StreamParts`, attached with
`streamcore_video_attach_stream` before `start`; the encoder, attached with
`streamcore_video_attach_encoder` in `EncoderFactory::create`), which the stream holds for as long
as it may call it: the queue and `VideoOut` until `Closed` (the wake holds a clone of the queue for
the call), the encoder while its session is current, the capture backend while a lease's weak
reference to it upgrades. A NULL function keeps the UniFFI call. Every rule above applies to the C
forms unchanged. A panic inside a C call does not unwind into native code: it is caught, the
stream fails with `stream.output`, and the call returns its "not taken" value. A permit released
after a drain's last check is posted when the drain ends. `streamcore_video_encoded` is the crate's only `unsafe` code: it reads the
caller's output struct and byte ranges for the duration of the call; the other exports need
`allow(unsafe_code)` only for `no_mangle`. Swift helpers are in
`StreamCoreVideo/Sources/StreamCoreVideo/NativeFastPath.swift`; a product that builds the bindings
into its own C module copies that file with its module import renamed and adds the header to its
module map.

## Binding names

UniFFI 0.32.2 shapes three names. The encoder trait is `StreamEncoder` (Rust keeps `Encoder` as an
alias): Swift's standard library has its own `Encoder`, and a Swift-only rename of a foreign trait
changes an FFI symbol and fails to link. `CaptureBackend::open` is `openCapture` in Swift, because
the escaped keyword would otherwise land in the C header. The async `close()` is `closeStream()` in
Kotlin. Generated Swift for async foreign-trait methods needs Swift 5 language mode (it fails
Swift 6's sending checks); the stream traits are synchronous and unaffected.
