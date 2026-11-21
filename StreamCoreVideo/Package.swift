// swift-tools-version: 6.0
// The video control plane (crates/streamcore-video) for Swift: the binding smoke test of
// docs/STREAMCORE-VIDEO.md 3.8 (an async foreign-trait round trip and a stream driven through
// Swift adapters). Run scripts/build-streamcore-video-apple.sh first; it builds the XCFramework
// into Artifacts/ and the UniFFI bindings into Sources/StreamCoreVideo/Generated/.
import PackageDescription

let package = Package(
    name: "StreamCoreVideo",
    platforms: [.macOS("15.0"), .iOS("18.0")],
    products: [
        .library(name: "StreamCoreVideo", targets: ["StreamCoreVideo"]),
        // The Apple host media code (design 4.1): capture, compositors, the surface store,
        // the VideoToolbox encoder and its probe.
        .library(name: "StreamCoreAppleHost", targets: ["StreamCoreAppleHost"]),
        // The Apple client media code (design 5): VideoToolbox decode, the presenter and
        // the display paths, on iOS and macOS.
        .library(name: "StreamCoreAppleClient", targets: ["StreamCoreAppleClient"]),
    ],
    targets: [
        .binaryTarget(name: "streamcore_videoFFI", path: "Artifacts/StreamCoreVideoFFI.xcframework"),
        // UniFFI 0.32.2's Swift for async foreign-trait methods (`AsyncProbe`) fails Swift 6's
        // sending checks (SendingClosureRisksDataRace), so the generated bindings build in
        // Swift 5 mode. The tests stay in Swift 6.
        .target(name: "StreamCoreVideo", dependencies: ["streamcore_videoFFI"],
                swiftSettings: [.swiftLanguageMode(.v5)]),
        .testTarget(name: "StreamCoreVideoTests", dependencies: ["StreamCoreVideo"]),
        .target(name: "StreamCoreAppleHost", dependencies: ["StreamCoreVideo"]),
        .testTarget(name: "StreamCoreAppleHostTests", dependencies: ["StreamCoreAppleHost"]),
        .target(name: "StreamCoreAppleClient", dependencies: ["StreamCoreVideo"]),
        // A macOS command-line probe for the accessory path (design 8.4): switches a cabled
        // Android device into accessory mode and serves a secure echo channel over it.
        .executableTarget(name: "AccessoryProbe", dependencies: ["StreamCoreAppleHost"]),
        .testTarget(name: "StreamCoreAppleClientTests", dependencies: ["StreamCoreAppleClient", "StreamCoreAppleHost"]),
    ]
)
