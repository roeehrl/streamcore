#if os(macOS)
import CoreMedia
import CoreVideo
import Foundation
@preconcurrency import ScreenCaptureKit
import StreamCoreVideo
import Synchronization
import Testing
@testable import StreamCoreAppleHost

/// A stand-in for `SCStream`: every operation completes from another queue.
private final class FakeStream: ScreenCaptureStream, @unchecked Sendable {
    let starts = Mutex(0), updates = Mutex<[SCStreamConfiguration]>([]), stops = Mutex(0)
    func startCapture(completionHandler: (@Sendable ((any Error)?) -> Void)?) {
        starts.withLock { $0 += 1 }
        DispatchQueue.global().async { completionHandler?(nil) }
    }
    func updateConfiguration(_ streamConfig: SCStreamConfiguration, completionHandler: (@Sendable ((any Error)?) -> Void)?) {
        updates.withLock { $0.append(streamConfig) }
        DispatchQueue.global().async { completionHandler?(nil) }
    }
    func stopCapture(completionHandler: (@Sendable ((any Error)?) -> Void)?) {
        stops.withLock { $0 += 1 }
        DispatchQueue.global().async { completionHandler?(nil) }
    }
}

private final class Trace: FrameTrace, @unchecked Sendable {
    let replacedCount = Mutex(0), submittedCount = Mutex(0)
    func submitted(sequence: UInt64, h0: UInt64, h1: UInt64, h3: UInt64, resubmit: Resubmit) { submittedCount.withLock { $0 += 1 } }
    func encoded(sequence: UInt64, keyframe: Bool, h4: UInt64) {}
    func encoderDrop(sequence: UInt64) {}
    func replaced(h0: UInt64, h1: UInt64) { replacedCount.withLock { $0 += 1 } }
    func count(_ counter: FrameCounter) {}
    func note(_ key: String, _ value: String) {}
}

private func nv12(width: Int, height: Int, step: Int) throws -> CVPixelBuffer {
    var buffer: CVPixelBuffer?
    try #require(CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &buffer) == kCVReturnSuccess)
    let image = try #require(buffer)
    CVPixelBufferLockBaseAddress(image, [])
    defer { CVPixelBufferUnlockBaseAddress(image, []) }
    let y = CVPixelBufferGetBaseAddressOfPlane(image, 0)!.assumingMemoryBound(to: UInt8.self)
    let stride = CVPixelBufferGetBytesPerRowOfPlane(image, 0)
    for row in 0..<height {
        for column in 0..<width { y[row * stride + column] = UInt8(truncatingIfNeeded: 40 + ((column / 8 + row / 8 + step) & 1) * 160) }
    }
    memset(CVPixelBufferGetBaseAddressOfPlane(image, 1)!, 0x80,
           CVPixelBufferGetBytesPerRowOfPlane(image, 1) * CVPixelBufferGetHeightOfPlane(image, 1))
    for (key, value) in [(kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2),
                         (kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2),
                         (kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2)] {
        CVBufferSetAttachment(image, key, value, .shouldPropagate)
    }
    return image
}

private func waitUntil(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now + .seconds(5)
    while !condition() {
        guard ContinuousClock.now < deadline else { throw CancellationError() }
        try await Task.sleep(for: .milliseconds(5))
    }
}

/// The host's parts together, as a product wires them: the ScreenCaptureKit backend (with a
/// stand-in stream), the stream and observer queues, the FFI meter, the surface store and
/// the VideoToolbox factory on StreamCore's executor, per-frame calls through the C ABI.
@Suite(.serialized) struct ScreenCaptureStreamTests {
    private final class Jobs: @unchecked Sendable {
        let kept = Mutex<[ObserverJob]>([])
    }

    @Test(.timeLimit(.minutes(1))) func aStreamRunsOnTheHostsPartsThroughTheCABI() async throws {
        let queue = DispatchQueue(label: "streamcore.applehost.test.capture", qos: .userInteractive)
        let surfaces = SurfaceStore()
        let trace = Trace()
        let meter = FFIMeter()
        let configuration = SCStreamConfiguration()
        configuration.queueDepth = 5
        configuration.minimumFrameInterval = CMTime(value: 1, timescale: 60)
        let backend = ScreenCaptureBackend(configuration: configuration, surfaces: surfaces, trace: trace, meter: meter)
        let stream = FakeStream()
        backend.attach(stream)
        #expect(backend.caps().poolDepth == 5)
        let factory = VideoToolboxEncoderFactory(surfaces: surfaces, trace: trace, meter: meter)
        let port = StreamPort()
        let serial = StreamSerialQueue(queue: queue, port: port, meter: meter)
        let jobs = Jobs()
        let spec = CaptureSpec(target: .display(displayId: 1), mode: .control, pixelWidth: 320, pixelHeight: 192,
                               minIntervalNs: 16_666_667, format: .nv12, color: .source, cursor: .composited,
                               queueDepth: 5, adapterLuid: nil)
        let plan = StreamPlan(mode: .control, codec: .h264, encodeWidth: 320, encodeHeight: 192, fps: 60,
                              bitrateBps: 2_000_000, convert: .identity, encoderInput: .nv12, encoder: "VideoToolbox",
                              colorOut: ColorTags(primaries: .bt709, transfer: .bt709, matrix: .bt709, fullRange: false),
                              colorMapping: ColorMapping(allowP3: true, srgbTransferAsBt709: false),
                              extensions: [], permitTarget: 1, recovery: [.idr], onSourceGone: .fail, mediumCapBps: nil,
                              adapterLuid: nil, refreshIntervalNs: nil,
                              pacing: PacingPolicy(refreshAligned: false, adaptivePermits: true, immediateRecovery: true))
        let output = VideoOutput.discard(policy: SinkPolicy(bounds: true, shedDiscardable: true, deltaAgeBound: false,
                                                            bulkBucket: true, timingEvery: 0, frameTimingEvery: 0))
        let controller = try StreamController(parts: StreamParts(
            streamId: 1, spec: spec, plan: plan, port: port, queue: serial, capture: backend, converter: nil,
            encoders: factory, output: output,
            observer: StreamObserverQueue(queue: queue) { job in jobs.kept.withLock { $0.append(job) } }, timings: nil))
        #expect(StreamCoreNative.attach(port: port.nativeKey(), queue: serial, wake: StreamSerialQueue.nativeWake,
                                        capture: backend, release: ScreenCaptureBackend.nativeRelease))
        backend.nativePort = port.nativeKey()
        _ = controller.start()
        try await waitUntil { controller.phase() == .running }
        #expect(stream.starts.withLock { $0 } == 1)
        for n in 1...6 {
            nonisolated(unsafe) let image = try nv12(width: 320, height: 192, step: n)
            let now = MediaClock.now()
            queue.async { backend.deliver(image, h0: now, h1: now) }
            try await waitUntil { controller.stats().encoded >= UInt64(n) }
        }
        let stats = controller.stats()
        #expect(stats.framesReceived == 6 && stats.encoded == 6 && stats.encoderDrops == 0)
        #expect(trace.submittedCount.withLock { $0 } == 6 && trace.replacedCount.withLock { $0 } == 0)

        // A live change (frame interval, cursor) reaches the stream's configuration.
        _ = controller.update(update: StreamUpdate(
            rate: nil, capture: CaptureUpdate(minIntervalNs: 33_333_333, region: nil, cursor: .hidden),
            encodeWidth: nil, encodeHeight: nil))
        try await waitUntil { stream.updates.withLock { !$0.isEmpty } }
        let updated = try #require(stream.updates.withLock { $0.last })
        #expect(!updated.showsCursor && abs(updated.minimumFrameInterval.seconds - 1.0 / 30) < 0.001)

        #expect(await controller.close() == .closed)
        #expect(stream.stops.withLock { $0 } == 1)
        #expect(surfaces.count == 0, "every surface was released")
        // The observer queue delivered the stream's start and close as separate blocks.
        queue.sync {}
        let kept = jobs.kept.withLock { $0 }
        #expect(kept.contains { if case .started = $0 { true } else { false } })
        #expect(kept.contains { if case .closed = $0 { true } else { false } })
        // The meter attributed FFI time to the frames that were sent.
        #expect(meter.perFrame.cpu.count >= 4)
        #expect(meter.summary["ffi_frames"] != nil)
    }

    @Test func refreshIntervalsComeFromTheDisplayRate() {
        #expect(DisplayRefresh.interval(fps: 60) == 16_666_667)
        #expect(DisplayRefresh.interval(fps: 144) == 6_944_444)
        #expect(DisplayRefresh.interval(fps: 0) == nil && DisplayRefresh.interval(fps: .nan) == nil)
        #expect(DisplayRefresh.display(containing: .null) == nil)
    }
}
#endif
