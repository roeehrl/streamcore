#!/usr/bin/env bash
# Builds the video control plane for Apple platforms: StreamCoreVideo/Artifacts/StreamCoreVideoFFI.xcframework
# (macOS arm64+x86_64, iOS arm64, iOS Simulator arm64+x86_64, static libraries, with the
# `ffi-probe` binding smoke surface) and the UniFFI Swift bindings for streamcore-video and the
# streamcore-media types it uses in StreamCoreVideo/Sources/StreamCoreVideo/Generated, then runs
# the Swift binding tests against the XCFramework and compiles the package for iOS. Deployment
# targets: macOS 15, iOS 18; override with STREAMCORE_VIDEO_MACOS_TARGET /
# STREAMCORE_VIDEO_IOS_TARGET. Set STREAMCORE_VIDEO_SKIP_TESTS=1 to only build.
set -euo pipefail
CORE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$CORE_ROOT"
CORE_VERSION="$(sed -n 's/^channel = "\([^"]*\)"/\1/p' rust-toolchain.toml)"
CORE_CARGO="$(rustup which --toolchain "$CORE_VERSION" cargo)"
export PATH="$(dirname "$CORE_CARGO"):$PATH"
export CARGO_PROFILE_RELEASE_STRIP=none
export MACOSX_DEPLOYMENT_TARGET="${STREAMCORE_VIDEO_MACOS_TARGET:-15.0}"
export IPHONEOS_DEPLOYMENT_TARGET="${STREAMCORE_VIDEO_IOS_TARGET:-18.0}"
APPLE_TARGETS=(aarch64-apple-darwin x86_64-apple-darwin aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios)
INSTALLED="$(rustup target list --installed --toolchain "$CORE_VERSION")"
for TARGET in "${APPLE_TARGETS[@]}"; do
    grep -qx "$TARGET" <<<"$INSTALLED" || rustup target add --toolchain "$CORE_VERSION" "$TARGET"
done
FEATURES=ffi-probe

# Bindings come from the host library's metadata (UniFFI library mode); one Swift file per crate.
cargo build --package streamcore-video --features "$FEATURES,bindgen" --locked
BINDINGS="$CORE_ROOT/target/streamcore-video-apple-bindings"
GENERATED="$CORE_ROOT/StreamCoreVideo/Sources/StreamCoreVideo/Generated"
rm -rf "$BINDINGS" "$GENERATED" && mkdir -p "$BINDINGS" "$GENERATED"
cargo run --package streamcore-video --features "$FEATURES,bindgen" --locked --bin streamcore-video-bindgen -- \
    generate --library "$CORE_ROOT/target/debug/libstreamcore_video.dylib" \
    --language swift --no-format --out-dir "$BINDINGS"
cp "$BINDINGS/streamcore_video.swift" "$BINDINGS/streamcore_media.swift" "$GENERATED/"

for TARGET in "${APPLE_TARGETS[@]}"; do
    cargo build --package streamcore-video --features "$FEATURES" --lib --release --locked --target "$TARGET"
done
STAGE="$(mktemp -d "$CORE_ROOT/target/streamcore-video-apple.XXXXXX")"
mkdir -p "$STAGE/macos" "$STAGE/simulator" "$STAGE/headers"
cp "$BINDINGS/streamcore_videoFFI.h" "$BINDINGS/streamcore_mediaFFI.h" "$STAGE/headers/"
# One module map declaring both C modules the generated Swift files import.
cat "$BINDINGS/streamcore_mediaFFI.modulemap" > "$STAGE/headers/module.modulemap"
printf '\n' >> "$STAGE/headers/module.modulemap"
cat "$BINDINGS/streamcore_videoFFI.modulemap" >> "$STAGE/headers/module.modulemap"
lipo -create "target/aarch64-apple-darwin/release/libstreamcore_video.a" \
    "target/x86_64-apple-darwin/release/libstreamcore_video.a" -output "$STAGE/macos/libstreamcore_video.a"
lipo -create "target/aarch64-apple-ios-sim/release/libstreamcore_video.a" \
    "target/x86_64-apple-ios/release/libstreamcore_video.a" -output "$STAGE/simulator/libstreamcore_video.a"
xcodebuild -create-xcframework \
    -library "$STAGE/macos/libstreamcore_video.a" -headers "$STAGE/headers" \
    -library "$STAGE/simulator/libstreamcore_video.a" -headers "$STAGE/headers" \
    -library "target/aarch64-apple-ios/release/libstreamcore_video.a" -headers "$STAGE/headers" \
    -output "$STAGE/StreamCoreVideoFFI.xcframework" >/dev/null
# Replace the artifact only after every slice succeeded.
rm -rf "$CORE_ROOT/StreamCoreVideo/Artifacts/StreamCoreVideoFFI.xcframework"
mkdir -p "$CORE_ROOT/StreamCoreVideo/Artifacts"
mv "$STAGE/StreamCoreVideoFFI.xcframework" "$CORE_ROOT/StreamCoreVideo/Artifacts/"
rm -rf "$STAGE"
echo "StreamCoreVideo XCFramework ready: StreamCoreVideo/Artifacts/StreamCoreVideoFFI.xcframework"

if [[ "${STREAMCORE_VIDEO_SKIP_TESTS:-0}" != 1 ]]; then
    swift test --package-path StreamCoreVideo
    # Compile the package for iOS device and Simulator against those slices (unsigned).
    (cd StreamCoreVideo && xcodebuild build -scheme StreamCoreVideo -destination 'generic/platform=iOS' \
        -derivedDataPath "$CORE_ROOT/target/streamcore-video-ios" CODE_SIGNING_ALLOWED=NO -quiet)
    (cd StreamCoreVideo && xcodebuild build -scheme StreamCoreVideo -destination 'generic/platform=iOS Simulator' \
        -derivedDataPath "$CORE_ROOT/target/streamcore-video-ios" CODE_SIGNING_ALLOWED=NO -quiet)
fi
