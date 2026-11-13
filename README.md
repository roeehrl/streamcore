# StreamCore

StreamCore is the streaming engine shared by [MyCNC](https://codonic.dev) and
[Pad](https://codonic.dev/apps/pad): the secure connection between a computer
and a phone or tablet, the multiplexed lanes that run over it, sessions that
survive a change of network, the media path that carries the computer's
picture, and the input model that turns touches, pens and keys into input on
the computer.

It is written in Rust, with bindings for Swift and Kotlin, and is driven by
native capture and codec code on each platform.

**Status:** pre-release. The code was extracted from MyCNC's core (at MyCNC
commit `e666931`); MyCNC pins this repository as a submodule. Interfaces and the
wire protocol will change.

## Layout

| Path | What |
|---|---|
| `crates/streamcore-media` | `frames.v3` media headers, the typed binary lane frames and video stream continuity. No network dependencies. |
| `crates/streamcore-session` | Host-owned logical sessions that survive transport replacement, and resume references. |
| `crates/streamcore-transport` | Pinned identities, OPAQUE pairing, the versioned hello, QUIC and TLS/TCP sessions, the five bounded multiplexed lanes and the secure channel. Its `examples/` hold the two-machine `network_probe` and the `scheduling_probe`. |
| `crates/streamcore-input`, `crates/streamcore-input-ffi` | The input model (Pad's apps and MyCNC) and its JSON C ABI (`include/streamcore_input.h`). `crates/streamcore-input/CHANGES.md` is the change log apps read before re-pinning. |
| `crates/streamcore-video` | The video control plane ([design](docs/STREAMCORE-VIDEO.md)): backend traits (UniFFI foreign traits), one serial executor per stream, lifecycle, latest-frame retention and recovery, and the per-frame C ABI for foreign backends (`include/streamcore_video_native.h`). P1 core; no platform backend yet. |
| `StreamCoreInput/` | The input model's Swift package (XCFramework plus UniFFI bindings, built by a script below). |
| `StreamCoreVideo/` | The video crate's Swift binding smoke test (`scripts/build-streamcore-video-apple.sh`). |
| `protocol/` | [`LANES.md`](protocol/LANES.md) (lane scheduling and limits), [`BINARY.md`](protocol/BINARY.md) (binary frame layouts), [`STREAM-CODES.md`](protocol/STREAM-CODES.md) (stream wire codes), [`input/`](protocol/input/README.md) (input fixtures every binding runs). |
| `docs/` | [Secure channel](docs/SECURE-CHANNEL.md), [sessions](docs/SESSIONS.md), [capture pipeline](docs/CAPTURE-PIPELINE.md), [input model](docs/INPUT-MODEL.md), [input ownership](docs/INPUT-OWNERSHIP.md), [video design](docs/STREAMCORE-VIDEO.md), [video threading rules](docs/VIDEO-THREADING.md). |
| `scripts/` | Input builds, the input change-log check, the scrub check and `ci.sh` (the Rust checks). |
| `ci/` | [Local CI](ci/local/README.md) jobs and the Linux lane; the [scheduling probe](ci/scheduling/README.md) harness. |

Every crate calls `uniffi::setup_scaffolding!()` and has a `uniffi.toml` naming its
own header (`<crate>FFI.h`) and Kotlin package (`uniffi.<crate>`). A product that
links several crates into one library (as MyCNC does) picks the shared Swift and C
module names in its own global bindgen config. Dependencies run one way:
`streamcore-transport` and `streamcore-video` depend on `streamcore-media`; nothing here names a product type.
The secure channel takes each product's lane rule as a `multiplex::LanePolicy`
(`channel::connect_secure_with` / `listen_secure_with`); `connect_secure` /
`listen_secure` use the binary kinds alone.

## Legacy identifiers

StreamCore began inside MyCNC, and some wire constants still carry MyCNC's name.
They stay, because changing them would break every device already paired:

| Constant | Where | Value |
|---|---|---|
| Session ALPN | `streamcore-transport/src/identity.rs` (`SESSION_ALPN`) | `mycnc/2` |
| Peer certificate name | `identity.rs` (`PEER_DNS_NAME`) | `peer.mycnc.invalid` |
| OPAQUE pairing contexts | `streamcore-transport/src/pairing.rs` | `mycnc-pairing-pin-v1`, `mycnc-pairing-persisted-v1`, `mycnc-pairing-v1:host:client:` |

Renaming any of them is a protocol change that needs a new transport major and a
migration for paired devices.

## Building and testing

Rust is pinned in `rust-toolchain.toml` (1.99.0) and dependencies in `Cargo.lock`.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo run --locked --example scheduling_probe -- self-test   # loopback, both transports
bash scripts/ci.sh                                           # all of the above plus the probe CLI check
```

Input model builds (see [docs/INPUT-MODEL.md](docs/INPUT-MODEL.md) for wiring them into an app):

| Target | Command | Needs | Output |
|---|---|---|---|
| Apple | `bash scripts/build-streamcore-input-apple.sh` | macOS with Xcode; the script adds the five Apple Rust targets | `StreamCoreInput/Artifacts/StreamCoreInputFFI.xcframework` and Swift bindings; runs the smoke program and Swift tests, compiles for iOS |
| Android | `bash scripts/build-streamcore-input-android.sh [abi…]` | An Android NDK (`ANDROID_NDK_HOME` or the newest in the SDK); installs `cargo-ndk` if missing | `target/streamcore-input-android/` (`jniLibs/<abi>/libstreamcore_input.so`, Kotlin bindings) |
| Windows | `STREAMCORE_WINDOWS_HOST=<ssh host> bash scripts/build-streamcore-input-windows.sh` | A Windows PC over SSH with Git Bash, rustup and the Visual Studio C++ tools; or `STREAMCORE_INPUT_WINDOWS_CROSS=1` to cross-compile with `cargo-xwin` (no tests) | `target/streamcore-input-windows/{x64,arm64}/streamcore_input_ffi.dll` and `streamcore_input.h` |

Video builds (see [docs/VIDEO-THREADING.md](docs/VIDEO-THREADING.md#builds-and-wiring) for wiring the client into an app):

| Target | Command | Needs | Output |
|---|---|---|---|
| Apple | `bash scripts/build-streamcore-video-apple.sh` | macOS with Xcode; the script adds the five Apple Rust targets | `StreamCoreVideo/Artifacts/StreamCoreVideoFFI.xcframework` and Swift bindings; runs the Swift binding tests, compiles for iOS |
| Android | `bash scripts/build-streamcore-video-android.sh [abi…]` | An Android NDK (`ANDROID_NDK_HOME` or the newest in the SDK); installs `cargo-ndk` if missing | `target/streamcore-video-android/` (`jniLibs/<abi>/libstreamcore_video.so`, Kotlin bindings for the video, media and transport crates); checks the bindings declare the client API |

`python3 scripts/check-input-changes.py` fails a change to the input model that
doesn't add an entry to `crates/streamcore-input/CHANGES.md`.
`scripts/scrub-check.sh` fails on private host names, addresses, personal data and
developer paths; it needs a private denylist outside the repository (see the script).
`ci/local/linux.sh` runs `scripts/ci.sh` in a Linux container (Docker or Colima).

## Example video host

`crates/streamcore-video/examples/video_host.rs` is a platform-neutral host for bringing up
clients: a real `StreamController` sending on a `SecureChannel`, a timer-driven capture and a
synthetic encoder that loops a 60-frame 640×360 H.264 clip
([`examples/data`](crates/streamcore-video/examples/data/README.md)). It runs on macOS, Windows
and Linux:

```sh
cargo run --release -p streamcore-video --example video_host -- --listen <lan-ip>:7700 [--pair-port 7701] [--fps 60]
```

Give it the LAN address clients reach. It prints a connect line for each 120-second pairing
window, `streamcore-video://<lan-ip>:7700?pair=7701&pin=<pin>&fp=<host fingerprint>`, which a
client pairs and connects with (the Android harness in `android/` takes it pasted). The client
advertises the `streamcore.video-demo.v1` capability; a `{"type":"keyframe"}` message of kind
`0x01` asks for a keyframe (the encoder jumps to the clip's next IDR). The `video_client`
example is a headless client for the same line: it pairs, connects and prints the decode
counters without a device (its decoder only counts):

```sh
cargo run --release -p streamcore-video --example video_client -- '<connect line>' --seconds 10
```

Both are built, not run, by `cargo test`.

## Licence

StreamCore is licensed under the [GNU Affero General Public License v3.0](LICENSE).
Contributions are accepted under the terms in [CONTRIBUTING.md](CONTRIBUTING.md).
