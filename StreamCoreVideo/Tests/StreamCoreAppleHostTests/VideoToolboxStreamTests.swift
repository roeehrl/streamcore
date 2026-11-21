#if os(macOS)
import CoreMedia
import CoreVideo
import Foundation
import StreamCoreVideo
import Testing
import VideoToolbox
@testable import StreamCoreAppleHost

private let bt709 = ColorTags(primaries: .bt709, transfer: .bt709, matrix: .bt709, fullRange: false)

/// A moving NV12 picture tagged BT.709.
private func picture(width: Int, height: Int, step: Int) throws -> CVPixelBuffer {
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

private final class Queue: SerialQueue, @unchecked Sendable {
    let queue = DispatchQueue(label: "streamcore.applehost.test.stream", qos: .userInteractive)
    let port: StreamPort
    init(port: StreamPort) { self.port = port }
    func wake() { queue.async { [port] in port.run() } }
    func wakeAfter(delayNs: UInt64, timer: UInt64) {
        queue.asyncAfter(deadline: .now() + .nanoseconds(Int(delayNs))) { [port] in port.runTimer(timer: timer) }
    }
}

/// A capture that registers real images in the surface store, as a capture backend does.
private final class Capture: CaptureBackend, @unchecked Sendable {
    let surfaces: SurfaceStore
    private let lock = NSLock()
    private var events: CaptureEvents?
    private var step = 0
    init(surfaces: SurfaceStore) { self.surfaces = surfaces }

    func caps() -> CaptureCaps {
        CaptureCaps(targetKinds: [.display], windowFit: false, liveRegionMove: false, cursorModes: [.composited],
                    surfaceKinds: [.ioSurface], poolDepth: 5, virtualDisplays: false, regionInSurface: true)
    }
    func enumerate() throws -> SourceCatalog { SourceCatalog(displays: [], windows: []) }
    func observeLayout(display: UInt32) -> DisplayLayout? { nil }
    func openCapture(spec: CaptureSpec, queue: SerialQueue, events: CaptureEvents) throws -> UInt64 {
        lock.withLock { self.events = events }
        return 7
    }
    func start(id: UInt64, op: OpToken) { lock.withLock { events }?.opDone(op: op, error: nil) }
    func update(id: UInt64, update: CaptureUpdate, op: OpToken) { lock.withLock { events }?.opDone(op: op, error: nil) }
    func stop(id: UInt64, op: OpToken) { lock.withLock { events }?.opDone(op: op, error: nil) }
    func release(surface: UInt64) { _ = surfaces.remove(surface) }

    func emit(on queue: DispatchQueue, width: Int, height: Int) throws {
        let (events, step) = lock.withLock { () -> (CaptureEvents?, Int) in defer { self.step += 1 }; return (self.events, self.step) }
        nonisolated(unsafe) let image = try picture(width: width, height: height, step: step)
        let now = MediaClock.now()
        queue.async { [surfaces] in
            let id = surfaces.insert(image, h0: now, h1: now)
            events?.frame(id: 7, member: nil, surface: SurfaceInfo(
                id: id, kind: .ioSurface, format: .nv12, width: UInt32(width), height: UInt32(height),
                color: SurfaceColor.tags(of: image), captureNs: now, sourcePresentNs: now, dirty: nil,
                rotation: .none, fence: .none), status: .complete)
        }
    }
}

private final class Observer: ObserverQueue, @unchecked Sendable {
    func post(job: ObserverJob) {}
}

private final class Trace: FrameTrace, @unchecked Sendable {
    private let lock = NSLock()
    private var kept: [String] = []
    var marks: [String] { lock.withLock { kept } }
    private func add(_ mark: String) { lock.withLock { kept.append(mark) } }
    func submitted(sequence: UInt64, h0: UInt64, h1: UInt64, h3: UInt64, resubmit: Resubmit) {
        add("submitted \(sequence) \(resubmit) \(h0 <= h1 && h1 <= h3)")
    }
    func encoded(sequence: UInt64, keyframe: Bool, h4: UInt64) { add("encoded \(sequence) \(keyframe)") }
    func encoderDrop(sequence: UInt64) { add("drop \(sequence)") }
    func replaced(h0: UInt64, h1: UInt64) {}
    func count(_ counter: FrameCounter) {}
    func note(_ key: String, _ value: String) {}
}

private func waitUntil(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now + .seconds(5)
    while !condition() {
        guard ContinuousClock.now < deadline else { throw CancellationError() }
        try await Task.sleep(for: .milliseconds(5))
    }
}

/// One stream on StreamCore's executor with the real VideoToolbox adapter: images from the
/// surface store in, encoded frames through the C ABI to the stream's sink.
@Suite(.serialized) struct VideoToolboxStreamTests {
    private struct Run {
        let controller: StreamController
        let queue: Queue
        let capture: Capture
        let factory: VideoToolboxEncoderFactory
    }

    private func run(codec: Codec = .h264, trace: Trace? = nil, authorize: @escaping @Sendable () -> Bool = { true }) throws -> Run {
        let port = StreamPort()
        let queue = Queue(port: port)
        let surfaces = SurfaceStore()
        let capture = Capture(surfaces: surfaces)
        let factory = VideoToolboxEncoderFactory(surfaces: surfaces, trace: trace, authorize: authorize)
        let spec = CaptureSpec(target: .display(displayId: 1), mode: .control, pixelWidth: 320, pixelHeight: 192,
                               minIntervalNs: 16_666_667, format: .nv12, color: .source, cursor: .composited,
                               queueDepth: 5, adapterLuid: nil)
        let plan = StreamPlan(mode: .control, codec: codec, encodeWidth: 320, encodeHeight: 192, fps: 60,
                              bitrateBps: 2_000_000, convert: .identity, encoderInput: .nv12, encoder: "VideoToolbox",
                              colorOut: bt709, colorMapping: ColorMapping(allowP3: true, srgbTransferAsBt709: false),
                              extensions: [], permitTarget: 3, recovery: [.idr], onSourceGone: .fail, mediumCapBps: nil,
                              adapterLuid: nil, refreshIntervalNs: nil,
                              pacing: PacingPolicy(refreshAligned: false, adaptivePermits: true, immediateRecovery: true))
        let output = VideoOutput.discard(policy: SinkPolicy(bounds: true, shedDiscardable: true, deltaAgeBound: false,
                                                            bulkBucket: true, timingEvery: 0, frameTimingEvery: 0))
        let controller = try StreamController(parts: StreamParts(
            streamId: 1, spec: spec, plan: plan, port: port, queue: queue, capture: capture, converter: nil,
            encoders: factory, output: output, observer: Observer(), timings: nil))
        return Run(controller: controller, queue: queue, capture: capture, factory: factory)
    }

    @Test(.timeLimit(.minutes(1)), arguments: [Codec.h264, .hevc])
    func framesFromTheSurfaceStoreAreEncodedAndSent(codec: Codec) async throws {
        let trace = Trace()
        let r = try run(codec: codec, trace: trace)
        _ = r.controller.start()
        try await waitUntil { r.controller.phase() == .running }
        for n in 1...4 {
            try r.capture.emit(on: r.queue.queue, width: 320, height: 192)
            try await waitUntil { r.controller.stats().encoded >= UInt64(n) }
        }
        let stats = r.controller.stats()
        // The codec configuration and the keyframe, then three deltas.
        #expect(stats.packetsSent == 5)
        #expect(stats.encoderDrops == 0 && stats.packetsRefused == 0)
        let encoder = try #require(r.factory.sessions.last)
        #expect(encoder.codec == codec && encoder.info().name == "VideoToolbox")
        // Every frame was marked at submit (H0 ≤ H1 ≤ H3) and at its output; the first is the keyframe.
        let marks = trace.marks
        #expect(marks.filter { $0.hasPrefix("submitted") }.count == 4)
        #expect(marks.allSatisfy { !$0.hasPrefix("submitted") || $0.hasSuffix("none true") })
        #expect(marks.contains("encoded 1 true") && marks.contains("encoded 4 false"))
        #expect(await r.controller.close() == .closed)
        // The stream released every surface it was given.
        #expect(r.capture.surfaces.count == 0)
    }

    /// Once the product's authority lapses, no output reaches StreamCore: each is dropped.
    @Test(.timeLimit(.minutes(1))) func outputsAreDroppedOnceAuthorityLapses() async throws {
        let allowed = Allowed()
        let trace = Trace()
        let r = try run(trace: trace) { allowed.value }
        _ = r.controller.start()
        try await waitUntil { r.controller.phase() == .running }
        try r.capture.emit(on: r.queue.queue, width: 320, height: 192)
        try await waitUntil { r.controller.stats().encoded >= 1 }
        allowed.value = false
        try r.capture.emit(on: r.queue.queue, width: 320, height: 192)
        try await waitUntil { r.controller.stats().encoderDrops >= 1 }
        #expect(r.controller.stats().encoded == 1)
        #expect(trace.marks.contains { $0.hasPrefix("drop ") })
        _ = await r.controller.close()
    }

    @Test func aSecondSubmitOfAnImageIsLabelledByWhyStreamCoreSentIt() throws {
        func opts(keyframe: Bool = false, refresh: Bool = false, keepWarm: Bool = false) -> SubmitOpts {
            SubmitOpts(submitSeq: 1, ptsNs: 0, captureNs: 0, forceKeyframe: keyframe, ltrRefresh: refresh,
                       ltrAcknowledged: nil, keepWarmRepeat: keepWarm)
        }
        #expect(Resubmit(again: false, opts: opts(keepWarm: true)) == .none)
        #expect(Resubmit(again: true, opts: opts(keepWarm: true)) == .keepWarm)
        #expect(Resubmit(again: true, opts: opts(keyframe: true)) == .recovery)
        #expect(Resubmit(again: true, opts: opts(refresh: true)) == .recovery)
        #expect(Resubmit(again: true, opts: opts()) == .skipReencode)

        // The store tells the encoder when an image comes a second time.
        let surfaces = SurfaceStore()
        let id = surfaces.insert(try picture(width: 64, height: 64, step: 0), h0: 1, h1: 2)
        #expect(surfaces.submit(id)?.again == false)
        #expect(surfaces.submit(id)?.again == true)
        #expect(surfaces.remove(id) != nil && surfaces.submit(id) == nil && surfaces.count == 0)
    }

    @Test func colourTagsRoundTripThroughVideoToolboxProperties() throws {
        let image = try picture(width: 64, height: 64, step: 0)
        let tags = SurfaceColor.tags(of: image)
        #expect(tags == bt709)
        let properties = SurfaceColor.properties(tags)
        #expect(properties.primaries == kCVImageBufferColorPrimaries_ITU_R_709_2)
        #expect(properties.transfer == kCVImageBufferTransferFunction_ITU_R_709_2)
        #expect(properties.matrix == kCVImageBufferYCbCrMatrix_ITU_R_709_2)
        // Untagged values keep the session's defaults.
        let none = SurfaceColor.properties(ColorTags(primaries: .unspecified, transfer: .unspecified, matrix: .unspecified, fullRange: false))
        #expect(none.primaries == nil && none.transfer == nil && none.matrix == nil)
    }
}

private final class Allowed: @unchecked Sendable {
    private let lock = NSLock()
    private var allowed = true
    var value: Bool {
        get { lock.withLock { allowed } }
        set { lock.withLock { allowed = newValue } }
    }
}
#endif
