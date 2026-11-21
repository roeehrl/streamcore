#if os(macOS)
import AVFoundation
import CoreMedia
import CoreVideo
import Foundation
@preconcurrency import ScreenCaptureKit
import StreamCoreAppleHost
import StreamCoreVideo
import Synchronization
import Testing
@testable import StreamCoreAppleClient

private final class FakeStream: ScreenCaptureStream, @unchecked Sendable {
    func startCapture(completionHandler: (@Sendable ((any Error)?) -> Void)?) { DispatchQueue.global().async { completionHandler?(nil) } }
    func updateConfiguration(_ streamConfig: SCStreamConfiguration, completionHandler: (@Sendable ((any Error)?) -> Void)?) {
        DispatchQueue.global().async { completionHandler?(nil) }
    }
    func stopCapture(completionHandler: (@Sendable ((any Error)?) -> Void)?) { DispatchQueue.global().async { completionHandler?(nil) } }
}

/// A display that records what it is given.
private final class RecordingDisplay: VideoDisplay, @unchecked Sendable {
    let shown = Mutex<[(pts: CMTime, width: Int)]>([])
    var path: VideoDisplays.Path { .layer }
    var isReady: Bool { true }
    var isFailed: Bool { false }
    func enqueue(_ sample: sending CMSampleBuffer) {
        let width = CMSampleBufferGetImageBuffer(sample).map(CVPixelBufferGetWidth) ?? 0
        let pts = CMSampleBufferGetPresentationTimeStamp(sample)
        shown.withLock { $0.append((pts, width)) }
    }
    func flush() {}
    func whenReady(on queue: DispatchQueue, _ block: @escaping @Sendable () -> Void) { queue.async(execute: block) }
}

private final class Jobs: ClientEvents, @unchecked Sendable {
    let kept = Mutex<[ClientJob]>([])
    func post(job: ClientJob) { kept.withLock { $0.append(job) } }
}

private func picture(step: Int) throws -> CVPixelBuffer {
    var buffer: CVPixelBuffer?
    try #require(CVPixelBufferCreate(nil, 320, 192, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &buffer) == kCVReturnSuccess)
    let image = try #require(buffer)
    CVPixelBufferLockBaseAddress(image, [])
    let y = CVPixelBufferGetBaseAddressOfPlane(image, 0)!.assumingMemoryBound(to: UInt8.self)
    let stride = CVPixelBufferGetBytesPerRowOfPlane(image, 0)
    for row in 0..<192 { for column in 0..<320 {
        y[row * stride + column] = UInt8(truncatingIfNeeded: 40 + ((column / 16 + row / 16 + step) & 1) * 150)
    } }
    memset(CVPixelBufferGetBaseAddressOfPlane(image, 1)!, 0x80,
           CVPixelBufferGetBytesPerRowOfPlane(image, 1) * CVPixelBufferGetHeightOfPlane(image, 1))
    CVPixelBufferUnlockBaseAddress(image, [])
    for (key, value) in [(kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2),
                         (kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2),
                         (kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2)] {
        CVBufferSetAttachment(image, key, value, .shouldPropagate)
    }
    return image
}

private func eventually(_ what: String, _ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now + .seconds(10)
    while !condition(), ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(5)) }
    #expect(condition(), "\(what)")
    try #require(condition())
}

/// A Mac host run and an Apple client stream in one process, over a pinned loopback
/// channel: ScreenCaptureKit's place is taken by pictures offered to the capture backend,
/// everything after it is real (VideoToolbox encode, StreamCore's sink and transport, the
/// client stream, VideoToolbox decode, the presenter).
@Suite(.serialized) struct ClientStreamLoopbackTests {
    @Test(.timeLimit(.minutes(2)), arguments: [Codec.h264, .hevc])
    func aHostRunReachesTheClientsDisplayThroughClientStream(codec: Codec) async throws {
        let hostIdentity = try PeerIdentity.generate(), clientIdentity = try PeerIdentity.generate()
        let options = SessionOptions(capabilities: ["frames.v4"], required: [])
        let listener = try await probeListen(identity: hostIdentity, trustedPeer: clientIdentity.certificate(), options: options)
        async let accepted = listener.accept()
        let clientChannel = try await connectSecure(
            identity: clientIdentity, trustedPeer: hostIdentity.certificate(),
            paths: [ChannelPath(address: listener.address(), transport: .tlsTcp, delayMs: 0)], options: options,
            cancellation: OperationCancellation())
        let hostChannel = try await accepted
        defer { hostChannel.close(); clientChannel.close(); listener.close() }
        #expect(clientChannel.framesV4() && hostChannel.framesV4())

        // The host.
        let hostQueue = DispatchQueue(label: "streamcore.loopback.host")
        let configuration = SCStreamConfiguration()
        configuration.queueDepth = 5
        configuration.minimumFrameInterval = CMTime(value: 1, timescale: 60)
        let output = VideoOutput.channel(channel: hostChannel, policy: SinkPolicy(
            bounds: true, shedDiscardable: true, deltaAgeBound: false, bulkBucket: true, timingEvery: 30, frameTimingEvery: 30),
            capacityBps: nil)
        let run = try AppleHostRun(
            config: HostRunConfig(streamID: 7, target: .display(displayId: 1), mode: .control, codec: codec, width: 320,
                                  height: 192, fps: 60, bitrateBps: 2_000_000, extensions: [.timing, .captureUs],
                                  pacing: PacingPolicy(refreshAligned: false, adaptivePermits: true, immediateRecovery: true)),
            configuration: configuration, queue: hostQueue, output: output)
        try #require(run.backend).attach(FakeStream())
        hostQueue.sync { _ = run.start() }
        try await eventually("the host run starts") { run.controller.phase() == .running }

        // The client.
        let display = RecordingDisplay()
        let decoder = VideoToolboxStreamDecoder()
        decoder.setDisplay(display)
        #expect(decoder.displayPath == .layer && decoder.presentTimesAreEstimated)
        let port = StreamPort()
        let jobs = Jobs()
        let stream = try ClientStream(parts: ClientParts(
            streamId: 7, codec: codec,
            color: ColorTags(primaries: .bt709, transfer: .bt709, matrix: .bt709, fullRange: false),
            mode: .responsive, port: port, queue: ClientStreamQueue(port: port), decoder: decoder, events: jobs,
            channel: clientChannel, timings: nil))
        let receiving = Task {
            // Video stays in Rust; anything else on the channel is not this test's.
            while (try? await stream.nextMessage(channel: clientChannel)) != nil {}
        }
        defer { receiving.cancel() }

        let frames = 12
        for step in 0..<frames {
            nonisolated(unsafe) let image = try picture(step: step)
            let now = MediaClock.now()
            hostQueue.sync { run.backend?.deliver(image, h0: now, h1: now) }
            try await eventually("frame \(step + 1) reaches the display") { display.shown.withLock { $0.count } >= step + 1 }
        }
        let stats = stream.stats()
        // The codec configuration and every frame.
        #expect(stats.framesReceived == UInt64(frames) + 1 && stats.decoded == UInt64(frames))
        #expect(stats.presented == UInt64(frames) && stats.presentedEstimated == UInt64(frames))
        #expect(stats.sessions == 1 && stats.decodeFailures == 0 && stats.keyframeRequests == 0)
        #expect(display.shown.withLock { $0.allSatisfy { $0.width == 320 } })
        // The client stream configured the decoder from the stream itself, and says how.
        let configured = jobs.kept.withLock { $0.compactMap { if case .configured(_, _, let info) = $0 { info } else { nil } } }
        #expect(configured.count == 1 && configured.first?.lowLatencyConfirmed == true)
        #expect(configured.first?.notes.contains { $0.contains(codec == .hevc ? "HEVC" : "H.264") } == true)
        // Feedback goes back on lane 0 from Rust (frames.v4).
        try await eventually("feedback is sent") { stream.stats().feedbackSent >= 1 }

        // A mirror shows the same decode.
        let mirror = RecordingDisplay()
        decoder.addDisplay(mirror)
        nonisolated(unsafe) let extra = try picture(step: frames)
        let now = MediaClock.now()
        hostQueue.sync { run.backend?.deliver(extra, h0: now, h1: now) }
        try await eventually("the mirror shows the next frame") { mirror.shown.withLock { $0.count } == 1 }
        #expect(display.shown.withLock { $0.count } == frames + 1)

        #expect(await stream.close() == .closed)
        hostQueue.sync { run.beginClose() }
        try await eventually("the host run closes") { hostQueue.sync { run.closed } != nil }
    }
}
#endif
