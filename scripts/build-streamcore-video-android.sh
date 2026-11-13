#!/usr/bin/env bash
# Builds the video client for Android: libstreamcore_video.so per ABI under
# target/streamcore-video-android/jniLibs/<abi>/ and the UniFFI Kotlin bindings under
# target/streamcore-video-android/kotlin/: packages uniffi.streamcore_video,
# uniffi.streamcore_media and uniffi.streamcore_transport (the video API uses the media and
# transport types: SecureChannel, TimedMessage, VideoWorkPermit). As on Apple, one library
# carries all three crates' scaffolding, and every generated file loads it through JNA by the
# name `streamcore_video`. A Kotlin MediaCodec adapter implements `StreamDecoder` against them
# (see docs/VIDEO-THREADING.md, "Client"). Copy both into the app as for the input model
# (docs/INPUT-MODEL.md, "Wiring into Pad"); the bindings also need kotlinx-coroutines (the
# async calls).
#
# ABIs default to arm64-v8a, armeabi-v7a and x86_64; pass others as arguments
# (e.g. `build-streamcore-video-android.sh arm64-v8a`). The NDK is $ANDROID_NDK_HOME or
# the newest ~/Library/Android/sdk/ndk/*; cargo-ndk is installed if missing (it points cc-rs,
# and so ring's C and assembly, at the NDK's clang for each target).
# STREAMCORE_VIDEO_ANDROID_API sets the minimum API level (default 28, PadDroid's minSdk).
set -euo pipefail
CORE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$CORE_ROOT"
CORE_VERSION="$(sed -n 's/^channel = "\([^"]*\)"/\1/p' rust-toolchain.toml)"
CORE_CARGO="$(rustup which --toolchain "$CORE_VERSION" cargo)"
export PATH="$(dirname "$CORE_CARGO"):$HOME/.cargo/bin:$PATH"
ABIS=("$@")
[[ ${#ABIS[@]} -gt 0 ]] || ABIS=(arm64-v8a armeabi-v7a x86_64)
API="${STREAMCORE_VIDEO_ANDROID_API:-28}"

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

OUT="$CORE_ROOT/target/streamcore-video-android"
rm -rf "$OUT" && mkdir -p "$OUT/jniLibs" "$OUT/kotlin"
TARGET_ARGS=()
for ABI in "${ABIS[@]}"; do TARGET_ARGS+=(-t "$ABI"); done
cargo ndk "${TARGET_ARGS[@]}" --platform "$API" -o "$OUT/jniLibs" \
    build --package streamcore-video --lib --release --locked
# cargo-ndk copies every cdylib of the build; keep only this library.
find "$OUT/jniLibs" -name '*.so' ! -name 'libstreamcore_video.so' -delete
for ABI in "${ABIS[@]}"; do
    [[ -f "$OUT/jniLibs/$ABI/libstreamcore_video.so" ]] || {
        echo "error: no libstreamcore_video.so for $ABI" >&2
        exit 1
    }
done

# Kotlin from the host library's metadata (UniFFI library mode): one file per crate.
cargo build --package streamcore-video --features bindgen --locked
case "$(uname -s)" in
    Darwin) HOST_LIBRARY="$CORE_ROOT/target/debug/libstreamcore_video.dylib" ;;
    *) HOST_LIBRARY="$CORE_ROOT/target/debug/libstreamcore_video.so" ;;
esac
cargo run --package streamcore-video --features bindgen --locked --bin streamcore-video-bindgen -- \
    generate --library "$HOST_LIBRARY" --language kotlin --no-format --out-dir "$OUT/kotlin"

# Smoke check of the bindings (nothing here compiles Kotlin: no kotlinc or Gradle project in
# the repository): the files exist, load the one library, and declare the client API.
KOTLIN="$OUT/kotlin/uniffi"
for CRATE in streamcore_video streamcore_media streamcore_transport; do
    FILE="$KOTLIN/$CRATE/$CRATE.kt"
    [[ -f "$FILE" ]] || { echo "error: missing $FILE" >&2; exit 1; }
    grep -q 'return "streamcore_video"' "$FILE" || {
        echo "error: $CRATE.kt does not load libstreamcore_video.so" >&2
        exit 1
    }
done
VIDEO_KT="$KOTLIN/streamcore_video/streamcore_video.kt"
for DECLARATION in 'public interface StreamDecoder' 'public interface DecoderEvents' \
    'public interface ClientEvents' 'open class ClientStream' 'public interface SerialQueue' \
    'open class StreamPort' 'suspend fun `closeStream`' 'suspend fun `nextMessage`'; do
    grep -qF "$DECLARATION" "$VIDEO_KT" || {
        echo "error: the Kotlin bindings lack \"$DECLARATION\"" >&2
        exit 1
    }
done

echo "Android video client ready:"
find "$OUT" -type f \( -name '*.so' -o -name '*.kt' \) | sed "s|$CORE_ROOT/||"
