# Input model changes

Pad's apps build `streamcore-input` (called `mycnc-input` before 2026-11-01) from one
pinned StreamCore commit and move only when they re-pin. This log is how they see what
changed since their pin. StreamCore's local CI (`local/input-changes`,
[`scripts/check-input-changes.py`](../../scripts/check-input-changes.py)) fails
any change to `crates/streamcore-input`, `crates/streamcore-input-ffi`, `protocol/input`,
`StreamCoreInput` or the input build scripts that doesn't add an entry here.

Newest first. Each entry names the date, says what changed for Pad, and whether
Pad needs to act when it re-pins. Entries before the move to this repository name
commits and pull requests in MyCNC's core repository, where the input model lived
until then.

## 2026-11-01 Moved to the StreamCore repository (MyCNC core `e666931`)

The input model, its C ABI, the `StreamCoreInput` package, `protocol/input` and the
build scripts now live in this repository. Code, names, behavior, the wire format,
the JSON method table and the C ABI version (1) are unchanged. **Pad must act when it
re-pins:** pin this repository instead of MyCNC's core, and call the build scripts at
their new paths:

| What | Before (MyCNC core) | After (StreamCore) |
| --- | --- | --- |
| Build scripts | `Scripts/build-streamcore-input-{apple,android,windows}.sh` | `scripts/build-streamcore-input-{apple,android,windows}.sh` |
| Change log check | `Scripts/check-input-changes.py` | `scripts/check-input-changes.py` |
| Windows PC | `MYCNC_CI_WINDOWS_HOST` (with a default) | `STREAMCORE_WINDOWS_HOST`, required unless `STREAMCORE_INPUT_WINDOWS_CROSS=1` |

`ci/local/streamcore-input-windows-remote.sh`, `crates/`, `protocol/input/` and
`StreamCoreInput/` keep their paths relative to the repository root.

## 2026-11-01 Renamed to StreamCore input (`994a84e`)

The input model is now part of StreamCore and every name changed; behavior, the
wire format, the JSON method table and the C ABI version (1) did not. **Pad must
act when it re-pins:** rename its consumers (package dependency, imports, build
script calls, the copied library and header names, `DllImport` and JNA library
names) to the new names below.

| What | Before | After |
| --- | --- | --- |
| Crate / lib | `crates/mycnc-input`, `mycnc-input` / `mycnc_input` | `crates/streamcore-input`, `streamcore-input` / `streamcore_input` |
| C ABI crate / lib | `crates/mycnc-input-ffi`, `mycnc-input-ffi` / `mycnc_input_ffi` | `crates/streamcore-input-ffi`, `streamcore-input-ffi` / `streamcore_input_ffi` |
| Bindgen binary | `mycnc-input-bindgen` | `streamcore-input-bindgen` |
| Swift package / module / product | `MyCNCInput` | `StreamCoreInput` (smoke `StreamCoreInputSmoke`, tests `StreamCoreInputTests`) |
| XCFramework / FFI module | `MyCNCInputFFI.xcframework`, `mycnc_inputFFI` | `StreamCoreInputFFI.xcframework`, `streamcore_inputFFI` |
| UniFFI Swift file / header | `mycnc_input.swift`, `mycnc_inputFFI.h` | `streamcore_input.swift`, `streamcore_inputFFI.h` |
| UniFFI C symbols | `uniffi_mycnc_input_*`, `ffi_mycnc_input_*` | `uniffi_streamcore_input_*`, `ffi_streamcore_input_*` |
| C ABI header | `mycnc_input.h` | `streamcore_input.h` |
| C ABI functions | `mycnc_input_call`, `mycnc_input_free`, `mycnc_input_abi_version` | `streamcore_input_call`, `streamcore_input_free`, `streamcore_input_abi_version` |
| C ABI constants | `MYCNC_INPUT_*` | `STREAMCORE_INPUT_*` |
| Kotlin package / Android library | `uniffi.mycnc_input`, `libmycnc_input.so` | `uniffi.streamcore_input`, `libstreamcore_input.so` |
| Windows DLL | `mycnc_input_ffi.dll` (+ `.dll.lib`, `.pdb`) | `streamcore_input_ffi.dll` (+ `.dll.lib`, `.pdb`) |
| Build scripts | `Scripts/build-mycnc-input-{apple,android,windows}.sh`, `ci/local/mycnc-input-windows-remote.sh` | `Scripts/build-streamcore-input-{apple,android,windows}.sh`, `ci/local/streamcore-input-windows-remote.sh` |
| Build outputs | `target/mycnc-input-{android,windows}/` | `target/streamcore-input-{android,windows}/` |
| Script variables | `MYCNC_INPUT_MACOS_TARGET`, `MYCNC_INPUT_IOS_TARGET`, `MYCNC_INPUT_SKIP_TESTS`, `MYCNC_INPUT_ANDROID_API`, `MYCNC_INPUT_WINDOWS_CROSS`, `MYCNC_INPUT_BASE` | `STREAMCORE_INPUT_MACOS_TARGET`, `STREAMCORE_INPUT_IOS_TARGET`, `STREAMCORE_INPUT_SKIP_TESTS`, `STREAMCORE_INPUT_ANDROID_API`, `STREAMCORE_INPUT_WINDOWS_CROSS`, `STREAMCORE_INPUT_BASE` |
| CI contexts | `local/mycnc-input-{apple,android,windows}` | `local/streamcore-input-{apple,android,windows}` |

`protocol/input` and its fixtures stay where they are (only path mentions in
fixture descriptions changed). Pad's own pin file is Pad's to rename.

## 2026-11-01 Change log

This file and its CI check. No code change; nothing to do when re-pinning.

## 2026-10-22 Pan and per-host shortcuts (#17, `a44cb33`)

Pan as a middle-button or Space drag; profiles carry per-host shortcuts.
Pad pins this commit.

## 2026-10-21 Krita preset (#16, `37fc251`)

The Krita preset zooms in with Ctrl+= and delivers gestures as shortcuts.

## 2026-10-21 Doc and fixture tidy-ups (#13, `ed581b8`)

Documentation and wire-fixture tidy-ups; no behavior change.

## 2026-10-11 Setup requests and edit timestamps (#6, `365e6ef`)

Setup requests, and monotonic profile edit timestamps.

## 2026-10-11 First version (#3, `6e444e3`)

The shared input model: v2 wire types, profiles and presets, KeySpec and
latches, two-finger math, with Apple, Android and Windows FFI outputs.
