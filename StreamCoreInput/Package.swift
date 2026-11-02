// swift-tools-version: 6.0
// The input model (crates/streamcore-input) for Swift: PadMac, PadPad and MyCNC.
// Run scripts/build-streamcore-input-apple.sh first; it builds the XCFramework into
// Artifacts/ and the UniFFI bindings into Sources/StreamCoreInput/Generated/.
import PackageDescription

let package = Package(
    name: "StreamCoreInput",
    platforms: [.macOS("15.0"), .iOS("18.0")],
    products: [.library(name: "StreamCoreInput", targets: ["StreamCoreInput"])],
    targets: [
        .binaryTarget(name: "streamcore_inputFFI", path: "Artifacts/StreamCoreInputFFI.xcframework"),
        .target(name: "StreamCoreInput", dependencies: ["streamcore_inputFFI"]),
        .executableTarget(name: "StreamCoreInputSmoke", dependencies: ["StreamCoreInput"]),
        .testTarget(name: "StreamCoreInputTests", dependencies: ["StreamCoreInput"]),
    ]
)
