#!/usr/bin/env bash
# Builds the input model for Apple platforms: StreamCoreInput/Artifacts/StreamCoreInputFFI.xcframework
# (macOS arm64+x86_64, iOS arm64, iOS Simulator arm64+x86_64, static libraries) and the
# UniFFI Swift bindings in StreamCoreInput/Sources/StreamCoreInput/Generated, then runs the Swift
# smoke program and tests against the XCFramework. Pad's apps depend on the StreamCoreInput
# package. Deployment targets follow Pad (macOS 15, iOS 18); override with
# STREAMCORE_INPUT_MACOS_TARGET / STREAMCORE_INPUT_IOS_TARGET. Set STREAMCORE_INPUT_SKIP_TESTS=1 to
# only build.
set -euo pipefail
CORE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$CORE_ROOT"
CORE_VERSION="$(sed -n 's/^channel = "\([^"]*\)"/\1/p' rust-toolchain.toml)"
CORE_CARGO="$(rustup which --toolchain "$CORE_VERSION" cargo)"
export PATH="$(dirname "$CORE_CARGO"):$PATH"
export CARGO_PROFILE_RELEASE_STRIP=none
export MACOSX_DEPLOYMENT_TARGET="${STREAMCORE_INPUT_MACOS_TARGET:-15.0}"
export IPHONEOS_DEPLOYMENT_TARGET="${STREAMCORE_INPUT_IOS_TARGET:-18.0}"
APPLE_TARGETS=(aarch64-apple-darwin x86_64-apple-darwin aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios)
INSTALLED="$(rustup target list --installed --toolchain "$CORE_VERSION")"
for TARGET in "${APPLE_TARGETS[@]}"; do
    grep -qx "$TARGET" <<<"$INSTALLED" || rustup target add --toolchain "$CORE_VERSION" "$TARGET"
done

# Bindings come from the host library's metadata (UniFFI library mode).
cargo build --package streamcore-input --features bindgen --locked
BINDINGS="$CORE_ROOT/target/streamcore-input-apple-bindings"
GENERATED="$CORE_ROOT/StreamCoreInput/Sources/StreamCoreInput/Generated"
rm -rf "$BINDINGS" && mkdir -p "$BINDINGS" "$GENERATED"
cargo run --package streamcore-input --features bindgen --locked --bin streamcore-input-bindgen -- \
    generate --library "$CORE_ROOT/target/debug/libstreamcore_input.dylib" \
    --language swift --no-format --out-dir "$BINDINGS"
cp "$BINDINGS/streamcore_input.swift" "$GENERATED/"

for TARGET in "${APPLE_TARGETS[@]}"; do
    cargo build --package streamcore-input --lib --release --locked --target "$TARGET"
done
STAGE="$(mktemp -d "$CORE_ROOT/target/streamcore-input-apple.XXXXXX")"
mkdir -p "$STAGE/macos" "$STAGE/simulator" "$STAGE/headers"
cp "$BINDINGS/streamcore_inputFFI.h" "$STAGE/headers/"
cp "$BINDINGS/streamcore_inputFFI.modulemap" "$STAGE/headers/module.modulemap"
lipo -create "target/aarch64-apple-darwin/release/libstreamcore_input.a" \
    "target/x86_64-apple-darwin/release/libstreamcore_input.a" -output "$STAGE/macos/libstreamcore_input.a"
lipo -create "target/aarch64-apple-ios-sim/release/libstreamcore_input.a" \
    "target/x86_64-apple-ios/release/libstreamcore_input.a" -output "$STAGE/simulator/libstreamcore_input.a"
xcodebuild -create-xcframework \
    -library "$STAGE/macos/libstreamcore_input.a" -headers "$STAGE/headers" \
    -library "$STAGE/simulator/libstreamcore_input.a" -headers "$STAGE/headers" \
    -library "target/aarch64-apple-ios/release/libstreamcore_input.a" -headers "$STAGE/headers" \
    -output "$STAGE/StreamCoreInputFFI.xcframework" >/dev/null
# Replace the artifact only after every slice succeeded.
rm -rf "$CORE_ROOT/StreamCoreInput/Artifacts/StreamCoreInputFFI.xcframework"
mkdir -p "$CORE_ROOT/StreamCoreInput/Artifacts"
mv "$STAGE/StreamCoreInputFFI.xcframework" "$CORE_ROOT/StreamCoreInput/Artifacts/"
rm -rf "$STAGE"
echo "StreamCoreInput XCFramework ready: StreamCoreInput/Artifacts/StreamCoreInputFFI.xcframework"

if [[ "${STREAMCORE_INPUT_SKIP_TESTS:-0}" != 1 ]]; then
    swift run --package-path StreamCoreInput -c release StreamCoreInputSmoke
    swift test --package-path StreamCoreInput
    # Compile the package for iOS device and Simulator against those slices (unsigned).
    (cd StreamCoreInput && xcodebuild build -scheme StreamCoreInput -destination 'generic/platform=iOS' \
        -derivedDataPath "$CORE_ROOT/target/streamcore-input-ios" CODE_SIGNING_ALLOWED=NO -quiet)
    (cd StreamCoreInput && xcodebuild build -scheme StreamCoreInput -destination 'generic/platform=iOS Simulator' \
        -derivedDataPath "$CORE_ROOT/target/streamcore-input-ios" CODE_SIGNING_ALLOWED=NO -quiet)
fi
