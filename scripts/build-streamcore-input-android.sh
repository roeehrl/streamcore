#!/usr/bin/env bash
# Builds the input model for Android: libstreamcore_input.so per ABI under
# target/streamcore-input-android/jniLibs/<abi>/ and the UniFFI Kotlin bindings under
# target/streamcore-input-android/kotlin/ (package uniffi.streamcore_input; they load the
# library through JNA). Copy both into the app (see docs/INPUT-MODEL.md).
#
# ABIs default to arm64-v8a, armeabi-v7a and x86_64; pass others as arguments
# (e.g. `build-streamcore-input-android.sh arm64-v8a`). The NDK is $ANDROID_NDK_HOME or
# the newest ~/Library/Android/sdk/ndk/*; cargo-ndk is installed if missing.
# STREAMCORE_INPUT_ANDROID_API sets the minimum API level (default 28, PadDroid's minSdk).
set -euo pipefail
CORE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$CORE_ROOT"
CORE_VERSION="$(sed -n 's/^channel = "\([^"]*\)"/\1/p' rust-toolchain.toml)"
CORE_CARGO="$(rustup which --toolchain "$CORE_VERSION" cargo)"
export PATH="$(dirname "$CORE_CARGO"):$HOME/.cargo/bin:$PATH"
ABIS=("$@")
[[ ${#ABIS[@]} -gt 0 ]] || ABIS=(arm64-v8a armeabi-v7a x86_64)
API="${STREAMCORE_INPUT_ANDROID_API:-28}"

if [[ -z "${ANDROID_NDK_HOME:-}" ]]; then
    SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
    ANDROID_NDK_HOME="$(ls -d "$SDK"/ndk/* 2>/dev/null | sort -V | tail -1 || true)"
fi
[[ -n "${ANDROID_NDK_HOME:-}" && -d "$ANDROID_NDK_HOME" ]] || {
    echo 'error: no Android NDK; install one with sdkmanager "ndk;<version>" or set ANDROID_NDK_HOME' >&2
    exit 1
}
export ANDROID_NDK_HOME
command -v cargo-ndk >/dev/null || cargo install --locked cargo-ndk

INSTALLED="$(rustup target list --installed --toolchain "$CORE_VERSION")"
for ABI in "${ABIS[@]}"; do
    case "$ABI" in
        arm64-v8a) TARGET=aarch64-linux-android ;;
        armeabi-v7a) TARGET=armv7-linux-androideabi ;;
        x86_64) TARGET=x86_64-linux-android ;;
        x86) TARGET=i686-linux-android ;;
        *) echo "error: unknown ABI $ABI" >&2; exit 1 ;;
    esac
    grep -qx "$TARGET" <<<"$INSTALLED" || rustup target add --toolchain "$CORE_VERSION" "$TARGET"
done

OUT="$CORE_ROOT/target/streamcore-input-android"
rm -rf "$OUT" && mkdir -p "$OUT/jniLibs" "$OUT/kotlin"
TARGET_ARGS=()
for ABI in "${ABIS[@]}"; do TARGET_ARGS+=(-t "$ABI"); done
cargo ndk "${TARGET_ARGS[@]}" --platform "$API" -o "$OUT/jniLibs" \
    build --package streamcore-input --lib --release --locked
# cargo-ndk copies every cdylib of the build; keep only this library.
find "$OUT/jniLibs" -name '*.so' ! -name 'libstreamcore_input.so' -delete

# Kotlin from the host library's metadata (UniFFI library mode).
cargo build --package streamcore-input --features bindgen --locked
case "$(uname -s)" in
    Darwin) HOST_LIBRARY="$CORE_ROOT/target/debug/libstreamcore_input.dylib" ;;
    *) HOST_LIBRARY="$CORE_ROOT/target/debug/libstreamcore_input.so" ;;
esac
cargo run --package streamcore-input --features bindgen --locked --bin streamcore-input-bindgen -- \
    generate --library "$HOST_LIBRARY" --language kotlin --no-format --out-dir "$OUT/kotlin"

echo "Android input model ready:"
find "$OUT" -type f \( -name '*.so' -o -name '*.kt' \) | sed "s|$CORE_ROOT/||"
