# streamcore-video-windows

The Windows backend for `streamcore-video`: Windows.Graphics.Capture on the chosen adapter,
an `ID3D11VideoProcessor` for BGRA→NV12 scaling and conversion, and a Media Foundation
hardware H.264 encoder. It compiles only on Windows (`cfg(windows)`). The `bench` feature adds
the measurement binary driven by `scripts/run-video-windows-bench.sh`.

## Backend rules

- **Threading.** Each stream's serial executor is a `DispatcherQueue` thread in a
  single-threaded apartment (STA), registered with MMCSS. Helper threads (the encoder event
  pump, bounded create and flush, encoder shutdown) run in the multithreaded apartment (MTA).
- **Nothing blocking on the executor at teardown.** Dropping an encoder hands
  `END_STREAMING`, `IMFShutdown` and `ShutdownObject` to an `mft-shutdown` thread, which joins
  the event thread and releases the MFT last; `MfEncoderFactory::wait_sessions` waits for them
  (the bench does, after close). Dropping the converter closes its fence thread's channel
  without joining it; `VpConverter::shutdown` joins, off the executor. An encoder whose
  creation fails after `ActivateObject` is shut down before the error returns (drivers limit
  encoder sessions per process).
- **Encoder failures end the session.** A failed MFT event, a failed `ProcessOutput`, a
  `GetEvent` that fails five times in a row (20 ms apart) or a lost device makes the event thread
  drop every frame in the encoder, the oldest with `DeviceLost` (the stream rebuilds the encoder)
  or `EncoderFailed { recoverable: false }`, fail later submits with the same error, and exit.
  Any other `ProcessInput` failure drops its frame and keeps its `NeedInput` credit. An
  invalidate whose flush overruns its bound drops the session's frames at once, so their
  permits do not hold admission. A keyframe request is applied when its input enters the MFT,
  never for a submit refused `Busy`.
- **Encoders must be asynchronous, free-threaded MFTs.** Only MFTs that set
  `MF_TRANSFORM_ASYNC` are used. Hardware encoder MFTs meet this by contract, which is why
  the encoder can be fed from the STA executor while its events are drained from an MTA
  helper thread. Opening a synchronous-only MFT fails with "not an asynchronous MFT", and
  `bench probe` reports `async` for every encoder it lists.
- **Regions.** WGC captures the whole display (`region_in_surface: false`); the video
  processor crops `ConvertJob::source`, so a region needs the convert path and follows live
  moves without touching the capture.
- **Capture frames outlive Rust's hold.** A WGC frame read by a recorded blit stays open
  until the converter's fence passes that blit, even when the executor releases the surface
  as soon as `convert` returns (`Surfaces::retire` / `reap`). Only values the GPU has completed
  close frames: a fence wait that times out (a TDR in progress) leaves them open for a later
  value, or until the slot table is dropped. Frames the pool dropped unseen are reported with
  `CaptureEvents::skipped`.
- **Resizes.** The frame that reveals a new source size arrives in a buffer of the old size;
  it is reported at its content size (clipped to the buffer) and the converter reads only that
  part. The capture adopts the new size once `Recreate` succeeds, so a failed `Recreate` is
  retried with the next frame. `stop` forgets the capture; a failed `start` unregisters and
  closes what it created.
- **Timer resolution (host rule).** Intel's Quick Sync H.264 MFT waits on the system timer.
  While a capture runs, the backend holds a reference-counted `TimerPeriod`: the first hold
  calls `timeBeginPeriod(1)` and opts the process out of timer-resolution power throttling
  (`PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION` set in the control mask and cleared in the
  state mask of `ProcessPowerThrottling`, read first with `GetProcessInformation`; no other
  bit changes, so EcoQoS or anything else the host set stays), so it also holds for a tray or
  service host with no visible window. The last release ends the period and puts the timer bits
  back as the first hold found them; nothing is held while idle, and `TimerPeriod::status`
  reports nothing applied then. A host that embeds the
  backend doesn't need to do anything; a host with its own encoder path should do the same.
  It costs power while streaming, which these measurements justify (720p, an Intel integrated
  GPU, a remote-desktop session at 32 Hz, 11 Nov 2026):

  | | encode p50 / p99 (probe, one in flight) | capture → encoded p50 / p99 (synthetic bench) |
  |---|---|---|
  | default 15.6 ms tick | 14.3 / 16.1 ms | 15.5 / 23.8 ms |
  | `TimerPeriod` held | 6.2 / 7.8 ms | 8.2 / 9.8 ms |
  | `TimerPeriod` held, console minimized | 6.1 / 7.6 ms | |

  The bench's `--timer-period off` measures without it.
- **Stream changes.** On `MF_E_TRANSFORM_STREAM_CHANGE` the output type is adopted and
  `ProcessOutput` called once more, as MF's contract asks. Quick Sync answers that retry with
  E_UNEXPECTED and sends the pending frame with its next `METransformHaveOutput`: E_UNEXPECTED
  or "need more input" on the retry means "wait for the event", not an error.
- **Parameter sets.** SPS and PPS go into avcC from the keyframe, or from the output type's
  `MF_MT_MPEG_SEQUENCE_HEADER` when the keyframe does not carry them. The VUI normaliser rewrites
  the colour description and `bitstream_restriction` and keeps the encoder's `video_format`
  (5, unspecified, when it wrote none).
- **Unsafe code** is denied crate-wide and allowed only in `src/win/sys/`, where the
  D3D11, DXGI and Media Foundation calls live. Every `unsafe` block there carries a
  `SAFETY:` comment.
- **Hardware paths are measured on a real Windows PC**, from an interactive desktop session.
  Capture and adapter enumeration fail without a logged-on user. Hosted CI runs clippy only
  (`.github/workflows/windows-clippy.yml`).
