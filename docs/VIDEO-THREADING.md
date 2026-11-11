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

`CaptureEvents::*`, `EncoderEvents::*`, `SinkReceipt::complete`, permit releases and every
`StreamController` method except `close_and_wait` enqueue an event and return in O(1). They touch
no stream state and call no native code except `SerialQueue::wake`, which `post` calls after it
has dropped the mailbox lock.

**Inline drain, frames only.** When `CaptureEvents::frame` arrives while no drain is active and
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
  `PermitReleased`);
- `VideoWorkQueue::close` (an atomic);
- `VideoSinkHandle::submit_encoded` (the handle's lock);
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
would retry it.

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
   conversions (Annex-B to AVCC, avcC/hvcC), call `VideoSinkHandle::submit_encoded` and post.
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
  table. Rust never forwards a fence to `submit`.
- `CaptureEvents::layout_changed` (a display mode, DPI, rotation or window size change) posts
  `ObserverJob::LayoutChanged { epoch, layout }`; the consumer re-plans, for example with a size
  update. The executor does not resize by itself.

## Clocks

Every `*_ns` value in a record is host-monotonic nanoseconds on one clock per host, converted
natively:

- **Windows:** QueryPerformanceCounter ticks × 10⁹ / QueryPerformanceFrequency. WGC's
  `SystemRelativeTime` is already in 100 ns units of that clock: multiply by 100.
- **macOS:** `mach_absolute_time` × timebase (the clock of `SCStreamFrameInfo.displayTime` and the
  host `CMClock`). It stops during sleep; `mach_continuous_time` does not, and would not match
  ScreenCaptureKit's or VideoToolbox's timestamps.
- **Linux:** `CLOCK_MONOTONIC`.

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
completion, a synchronous refusal included, comes back as a posted receipt; a receipt dropped
uncompleted counts as refused. From P2, `VideoSink` replaces `VideoOut`.

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
operation state machine; `stress.rs` seeded shuffles and many-thread runs. `StreamCoreVideo/`
runs the Swift binding smoke test.

## Binding names

UniFFI 0.32.2 shapes three names. The encoder trait is `StreamEncoder` (Rust keeps `Encoder` as an
alias): Swift's standard library has its own `Encoder`, and a Swift-only rename of a foreign trait
changes an FFI symbol and fails to link. `CaptureBackend::open` is `openCapture` in Swift, because
the escaped keyword would otherwise land in the C header. The async `close()` is `closeStream()` in
Kotlin. Generated Swift for async foreign-trait methods needs Swift 5 language mode (it fails
Swift 6's sending checks); the stream traits are synchronous and unaffected.
