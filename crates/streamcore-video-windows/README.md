# streamcore-video-windows

The Windows backend for `streamcore-video`: Windows.Graphics.Capture on the chosen adapter,
an `ID3D11VideoProcessor` for BGRA→NV12 scaling and conversion, and a Media Foundation
hardware H.264 encoder. It compiles only on Windows (`cfg(windows)`). The `bench` feature adds
the measurement binary driven by `scripts/run-video-windows-bench.sh`.

## Backend rules

- **Threading.** Each stream's serial executor is a `DispatcherQueue` thread in a
  single-threaded apartment (STA), registered with MMCSS. Helper threads (the encoder event
  pump, bounded create and flush) run in the multithreaded apartment (MTA).
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
  as soon as `convert` returns (`Surfaces::retire` / `reap`). Frames the pool dropped unseen
  are reported with `CaptureEvents::skipped`.
- **Timer resolution (host rule).** Intel's Quick Sync H.264 MFT waits on the system timer.
  While a capture runs, the backend holds a reference-counted `TimerPeriod`: the first hold
  calls `timeBeginPeriod(1)` and opts the process out of timer-resolution power throttling
  (`SetProcessInformation(ProcessPowerThrottling)`, `PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION`
  in the control mask, state 0), so it also holds for a tray or service host with no visible
  window. The last release undoes both, and nothing is held while idle. A host that embeds the
  backend doesn't need to do anything; a host with its own encoder path should do the same.
  It costs power while streaming, which these measurements justify (720p, an Intel integrated
  GPU, a remote-desktop session at 32 Hz, 11 Nov 2026):

  | | encode p50 / p99 (probe, one in flight) | capture → encoded p50 / p99 (synthetic bench) |
  |---|---|---|
  | default 15.6 ms tick | 14.3 / 16.1 ms | 15.5 / 23.8 ms |
  | `TimerPeriod` held | 6.2 / 7.8 ms | 8.2 / 9.8 ms |
  | `TimerPeriod` held, console minimized | 6.1 / 7.6 ms | |

  The bench's `--timer-period off` measures without it.
- **Stream changes.** On `MF_E_TRANSFORM_STREAM_CHANGE` the output type is adopted and the
  pending frame comes with the next `METransformHaveOutput`; calling `ProcessOutput` again at
  once fails with E_UNEXPECTED on Quick Sync.
- **Unsafe code** is denied crate-wide and allowed only in `src/win/sys/`, where the
  D3D11, DXGI and Media Foundation calls live. Every `unsafe` block there carries a
  `SAFETY:` comment.
- **Hardware paths are measured on a real Windows PC**, from an interactive desktop session.
  Capture and adapter enumeration fail without a logged-on user. Hosted CI runs clippy only
  (`.github/workflows/windows-clippy.yml`).
