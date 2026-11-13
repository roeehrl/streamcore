# Local CI

StreamCore's checks run on our own machines, not on hosted runners. A local runner
reads this commit's [`jobs.json`](jobs.json), runs each job from the repository
root and reports it to GitHub as a commit status named `local/…`.

| Context | Lane | What runs |
| --- | --- | --- |
| `local/scrub` | mac | [`scripts/scrub-check.sh`](../../scripts/scrub-check.sh) over the tracked files |
| `local/fmt` | mac | `cargo fmt --all --check` |
| `local/clippy` | mac | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` |
| `local/test` | mac | `cargo test --workspace --all-features --locked` |
| `local/scheduling` | mac | The [scheduling probe](../scheduling/README.md)'s loopback self-test and CLI check |
| `local/input-changes` | mac | [`scripts/check-input-changes.py`](../../scripts/check-input-changes.py): an input model change must add an entry to [`CHANGES.md`](../../crates/streamcore-input/CHANGES.md) |
| `local/streamcore-input-apple` | mac | [`scripts/build-streamcore-input-apple.sh`](../../scripts/build-streamcore-input-apple.sh) |
| `local/streamcore-input-android` | mac | [`scripts/build-streamcore-input-android.sh`](../../scripts/build-streamcore-input-android.sh) |
| `local/streamcore-video-android` | mac | [`scripts/build-streamcore-video-android.sh`](../../scripts/build-streamcore-video-android.sh): the video library per ABI and its Kotlin bindings, checked to declare the client API |
| `local/streamcore-video-android-app` | mac | [`scripts/build-streamcore-video-android-app.sh`](../../scripts/build-streamcore-video-android-app.sh): the video library, then the Gradle project in `android/` (the MediaCodec adapter and the harness app; compiles the generated Kotlin) |
| `local/streamcore-video-apple` | mac | [`scripts/build-streamcore-video-apple.sh`](../../scripts/build-streamcore-video-apple.sh): the video crate's Swift binding smoke test |
| `local/linux` | linux | [`linux.sh`](linux.sh): [`scripts/ci.sh`](../../scripts/ci.sh) in a linux container ([`linux.Dockerfile`](linux.Dockerfile)) |

Every job is an ordinary command; run it by hand the same way. The Windows input
build ([`scripts/build-streamcore-input-windows.sh`](../../scripts/build-streamcore-input-windows.sh),
with [`streamcore-input-windows-remote.sh`](streamcore-input-windows-remote.sh) on the PC)
is not a job yet: it needs `STREAMCORE_WINDOWS_HOST`, a Windows PC reachable over SSH
with key login, Git for Windows, rustup and Visual Studio C++ tools with a Windows SDK.
