#if os(macOS)
import CoreMedia
import CoreVideo
import Foundation
@preconcurrency import ScreenCaptureKit
import StreamCoreVideo
import Synchronization
import Testing
import VideoToolbox
@testable import StreamCoreAppleHost

private final class FakeStream: ScreenCaptureStream, @unchecked Sendable {
    let stops = Mutex(0)
    let updates = Mutex<[SCStreamConfiguration]>([])
    func startCapture(completionHandler: (@Sendable ((any Error)?) -> Void)?) {
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

private final class Notes: FrameTrace, @unchecked Sendable {
    let kept = Mutex<[String: String]>([:])
    func submitted(sequence: UInt64, h0: UInt64, h1: UInt64, h3: UInt64, resubmit: Resubmit) {}
    func encoded(sequence: UInt64, keyframe: Bool, h4: UInt64) {}
    func encoderDrop(sequence: UInt64) {}
    func replaced(h0: UInt64, h1: UInt64) {}
    func count(_ counter: FrameCounter) {}
    func note(_ key: String, _ value: String) { kept.withLock { $0[key] = value } }
}

private func nv12(step: Int = 0) throws -> CVPixelBuffer {
    var buffer: CVPixelBuffer?
    try #require(CVPixelBufferCreate(nil, 64, 64, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &buffer) == kCVReturnSuccess)
    let image = try #require(buffer)
    CVPixelBufferLockBaseAddress(image, [])
    for plane in 0..<CVPixelBufferGetPlaneCount(image) {
        memset(CVPixelBufferGetBaseAddressOfPlane(image, plane)!, Int32(0x60 + step * 8),
               CVPixelBufferGetBytesPerRowOfPlane(image, plane) * CVPixelBufferGetHeightOfPlane(image, plane))
    }
    CVPixelBufferUnlockBaseAddress(image, [])
    for (key, value) in [(kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2),
                         (kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2),
                         (kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2)] {
        CVBufferSetAttachment(image, key, value, .shouldPropagate)
    }
    return image
}

private func bgra(width: Int, height: Int, value: UInt8) throws -> CVPixelBuffer {
    var buffer: CVPixelBuffer?
    try #require(CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_32BGRA,
        [kCVPixelBufferIOSurfacePropertiesKey: [:], kCVPixelBufferMetalCompatibilityKey: true] as CFDictionary,
        &buffer) == kCVReturnSuccess)
    let image = try #require(buffer)
    CVPixelBufferLockBaseAddress(image, [])
    memset(CVPixelBufferGetBaseAddress(image)!, Int32(value), CVPixelBufferGetBytesPerRow(image) * height)
    CVPixelBufferUnlockBaseAddress(image, [])
    CVBufferSetAttachment(image, kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2, .shouldPropagate)
    CVBufferSetAttachment(image, kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_sRGB, .shouldPropagate)
    return image
}

private func waitUntil(seconds: Int = 5, _ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now + .seconds(seconds)
    while !condition() {
        guard ContinuousClock.now < deadline else { throw CancellationError() }
        try await Task.sleep(for: .milliseconds(5))
    }
}

private func discard() -> VideoOutput {
    VideoOutput.discard(policy: SinkPolicy(bounds: true, shedDiscardable: true, deltaAgeBound: false,
                                           bulkBucket: true, timingEvery: 0, frameTimingEvery: 0))
}

private func configuration() -> SCStreamConfiguration {
    let configuration = SCStreamConfiguration()
    configuration.queueDepth = 5
    configuration.minimumFrameInterval = CMTime(value: 1, timescale: 60)
    return configuration
}

private func config(target: CaptureTarget = .display(displayId: 1), codec: Codec = .h264, width: Int = 64, height: Int = 64) -> HostRunConfig {
    HostRunConfig(streamID: 7, target: target, mode: .control, codec: codec, width: width, height: height, fps: 60,
                  bitrateBps: 2_000_000,
                  pacing: PacingPolicy(refreshAligned: false, adaptivePermits: true, immediateRecovery: true))
}

/// The generic host run: one stream from an attached capture to StreamCore's sink, with the
/// stream's jobs handed to the product.
@Suite(.serialized) struct AppleHostRunTests {
    private final class Harness: @unchecked Sendable {
        let queue = DispatchQueue(label: "streamcore.applehost.test.run")
        let run: AppleHostRun
        let stream = FakeStream()
        let jobs = Mutex<[ObserverJob]>([])

        init(config: HostRunConfig, trace: Notes? = nil, authorize: @escaping @Sendable () -> Bool = { true },
             sessionFactory: @escaping VideoToolboxSession.Factory = VideoToolboxSession.create) throws {
            run = try AppleHostRun(config: config, configuration: configuration(), queue: queue, output: discard(),
                                   authorize: authorize, trace: trace, meter: trace == nil ? nil : FFIMeter(),
                                   sessionFactory: sessionFactory)
            try #require(run.backend).attach(stream)
            queue.sync { run.onJob = { [unowned self] job in self.jobs.withLock { $0.append(job) } } }
        }

        func result(of op: OpToken) async throws -> OpOutcome {
            var found: OpOutcome?
            try await waitUntil {
                found = jobs.withLock { $0.compactMap { if case .opResult(let done, let outcome) = $0, done.id == op.id { outcome } else { nil } }.first }
                return found != nil
            }
            return try #require(found)
        }

        func start() async throws {
            let op = queue.sync { run.start() }
            #expect(try await result(of: op) == .completed)
        }

        func offer(_ image: CVPixelBuffer) {
            nonisolated(unsafe) let image = image
            let now = MediaClock.now()
            queue.sync { run.backend?.deliver(image, h0: now, h1: now) }
        }

        func close() async throws {
            queue.sync { run.beginClose() }
            try await waitUntil { self.queue.sync { self.run.closed } != nil }
            #expect(queue.sync { run.closed } == .closed)
            #expect(stream.stops.withLock { $0 } == 1)
            #expect(run.surfaces.count == 0, "every surface released at close")
        }
    }

    @Test(.timeLimit(.minutes(1))) func aRunStartsStreamsUpdatesAndCloses() async throws {
        let notes = Notes()
        let h = try Harness(config: config(), trace: notes)
        try await h.start()
        #expect(h.queue.sync { h.run.codec } == .h264)
        for n in 1...4 {
            h.offer(try nv12(step: n))
            try await waitUntil { h.run.controller.stats().encoded >= UInt64(n) }
        }
        // A rate and cursor change: no restart.
        let op = h.queue.sync {
            h.run.update(StreamUpdate(rate: RateUpdate(bitrateBps: 1_000_000, fps: 30),
                                      capture: CaptureUpdate(minIntervalNs: 33_333_333, region: nil, cursor: .hidden),
                                      encodeWidth: nil, encodeHeight: nil))
        }
        #expect(try await h.result(of: op) == .completed)
        #expect(h.run.factory.sessions.count == 1)
        let stats = h.run.controller.stats()
        // Codec configuration and keyframe, then three deltas.
        #expect(stats.packetsSent == 5 && stats.encoderDrops == 0)
        try await h.close()
        let kept = h.jobs.withLock { $0 }
        #expect(kept.contains { if case .started = $0 { true } else { false } })
        #expect(kept.contains { if case .closed = $0 { true } else { false } })
        // The run's notes reached the product's trace at start and close.
        let written = notes.kept.withLock { $0 }
        #expect(written["pipeline"] == "streamcore" && written["codec"] == "h264" && written["encoded_size"] == "64x64")
        #expect(written["sc_encoded"] == "4" && written["ffi_frames"] != nil)
    }

    /// Once the product's authority lapses, the output is dropped and the sink revoked:
    /// nothing more is sent, whatever is offered afterwards.
    @Test(.timeLimit(.minutes(1))) func aLapsedAuthorityRevokesTheSink() async throws {
        let allowed = Mutex(true)
        let h = try Harness(config: config()) { allowed.withLock { $0 } }
        try await h.start()
        h.offer(try nv12())
        try await waitUntil { h.run.controller.stats().packetsSent == 2 }
        allowed.withLock { $0 = false }
        h.offer(try nv12(step: 1))
        try await Task.sleep(for: .milliseconds(300))
        h.run.requestKeyframe()
        h.offer(try nv12(step: 2))
        try await Task.sleep(for: .milliseconds(600))
        #expect(h.run.controller.stats().packetsSent == 2)
        #expect(h.run.controller.sinkState().unsentFrames == 0)
        try await h.close()
    }

    @Test(.timeLimit(.minutes(1))) func hevcThatCannotBeCreatedFallsBackToAnnouncedH264() async throws {
        let h = try Harness(config: config(codec: .hevc), sessionFactory: { width, height, codec, specification in
            if codec == kCMVideoCodecType_HEVC { return (kVTCouldNotFindVideoEncoderErr, nil) }
            return VideoToolboxSession.create(width: width, height: height, codec: codec, specification: specification)
        })
        try await h.start()
        #expect(h.queue.sync { h.run.codec } == .h264)
        h.offer(try nv12())
        try await waitUntil { h.run.controller.stats().encoded >= 1 }
        #expect(h.run.factory.sessions.last?.codec == .h264)
        try await h.close()
    }

    /// A region moves live: its origin and size, normalized to the captured content, become
    /// the stream's `sourceRect` with no restart. Without the content's size the run says
    /// it cannot move a region.
    @Test(.timeLimit(.minutes(1))) func aRegionMovesLiveWithinTheCapturedContent() async throws {
        var regional = config()
        regional.contentSize = CGSize(width: 1000, height: 500)
        let h = try Harness(config: regional)
        #expect(try #require(h.run.backend).caps().liveRegionMove)
        try await h.start()
        let op = h.queue.sync {
            h.run.update(StreamUpdate(rate: nil, capture: CaptureUpdate(
                minIntervalNs: nil, region: NormalizedRect(x: 0.25, y: 0.5, width: 0.5, height: 0.25), cursor: nil),
                encodeWidth: nil, encodeHeight: nil))
        }
        #expect(try await h.result(of: op) == .completed)
        let applied = try #require(h.stream.updates.withLock { $0.last })
        #expect(applied.sourceRect == CGRect(x: 250, y: 250, width: 500, height: 125))
        #expect(h.run.factory.sessions.count <= 1)
        try await h.close()
        #expect(!(try #require(try Harness(config: config()).run.backend).caps().liveRegionMove))
    }

    /// ScreenCaptureKit's suspended frames (the Mac locked, the display asleep) reach the
    /// product once when they begin and once when pictures return.
    @Test(.timeLimit(.minutes(1))) func suspensionIsReportedWhenItBeginsAndEnds() async throws {
        let h = try Harness(config: config())
        let seen = Mutex<[Bool]>([])
        h.queue.sync { h.run.onSuspended = { suspended in seen.withLock { $0.append(suspended) } } }
        try await h.start()
        let backend = try #require(h.run.backend)
        h.offer(try nv12())
        h.queue.sync { backend.status(.suspended); backend.status(.suspended); backend.status(.idle) }
        #expect(seen.withLock { $0 } == [true])
        h.offer(try nv12(step: 1))
        #expect(seen.withLock { $0 } == [true, false])
        try await h.close()
    }

    @Test func thePlanTakesOnlyFeaturesTheProbeConfirmsAndTheProductAllows() {
        func caps(ltr: FeatureSupport, layers: FeatureSupport, qp: Bool = true) -> EncoderCaps {
            var caps = VideoToolboxProbe.run(.hevc)
            caps.codec = .h264; caps.ltr = ltr; caps.temporalLayers = layers; caps.maxFrameQp = qp
            return caps
        }
        let both = AppleHostRun.planFeatures(codec: .h264, ltr: true, temporalLayers: true) { _ in caps(ltr: .confirmed, layers: .confirmed) }
        #expect(both.recovery.contains(.ltrRefresh) && both.extensions.contains(.temporalId))
        // Off in the product: unconfirmed, whatever the encoder does.
        let noLtr = AppleHostRun.planFeatures(codec: .h264, ltr: false, temporalLayers: true) { _ in caps(ltr: .confirmed, layers: .confirmed) }
        #expect(!noLtr.recovery.contains(.ltrRefresh) && noLtr.extensions.contains(.temporalId))
        // Only requested by the encoder: not used.
        let unconfirmed = AppleHostRun.planFeatures(codec: .h264, ltr: true, temporalLayers: true) { _ in caps(ltr: .requested, layers: .unsupported) }
        #expect(unconfirmed.recovery == [.idr] && !unconfirmed.extensions.contains(.temporalId))
        // Neither asked for: the probe is not run.
        let none = AppleHostRun.planFeatures(codec: .h264, ltr: false, temporalLayers: false) { _ in
            Issue.record("probed"); return caps(ltr: .confirmed, layers: .confirmed)
        }
        #expect(none.recovery == [.idr])

        // The maximum QP only with rate adaptation and a probe that confirmed it.
        var pacing = PacingPolicy(refreshAligned: true, adaptivePermits: true, immediateRecovery: true)
        #expect(AppleHostRun.withEncoderCaps(pacing, caps(ltr: .confirmed, layers: .confirmed)).maxFrameQp == 0)
        pacing.rateAdaptation = true
        #expect(AppleHostRun.withEncoderCaps(pacing, caps(ltr: .confirmed, layers: .confirmed)).maxFrameQp == 51)
        #expect(AppleHostRun.withEncoderCaps(pacing, caps(ltr: .confirmed, layers: .confirmed, qp: false)).maxFrameQp == 0)
        #expect(AppleHostRun.withEncoderCaps(pacing, nil).maxFrameQp == 0)
        #expect(AppleHostRun.permitTarget(pacing) == 1)
        pacing.adaptivePermits = false
        #expect(AppleHostRun.permitTarget(pacing) == 3)
    }

    @Test func aCanvasTargetNeedsACanvasAndAMetalCompositorThatCannotStartIsReported() throws {
        let canvasTarget = CaptureTarget.canvas(displayIds: [11, 12])
        #expect(throws: MediaError.self) {
            _ = try AppleHostRun(config: config(target: canvasTarget, width: 128, height: 48), configuration: configuration(),
                                 queue: DispatchQueue(label: "streamcore.applehost.test.nocanvas"), output: discard())
        }
        struct NoGPU: Error {}
        let canvas = HostCanvas(arrangement: CanvasArrangement(members: []), compositor: .metal,
                                makeStream: { _, _, _ in throw MediaError.Unsupported }, makeMetal: { _ in throw NoGPU() })
        #expect(throws: HostRunError.self) {
            _ = try AppleHostRun(config: config(target: canvasTarget, width: 128, height: 48), configuration: configuration(),
                                 queue: DispatchQueue(label: "streamcore.applehost.test.nogpu"), output: discard(), canvas: canvas)
        }
    }

    /// A combined canvas: the run opens one stream per member at its tile size, composes
    /// them and encodes the canvas.
    @Test(.timeLimit(.minutes(1)), arguments: [CanvasCompositorKind.metal, .cpu])
    func aCanvasRunComposesItsMembers(kind: CanvasCompositorKind) async throws {
        let queue = DispatchQueue(label: "streamcore.applehost.test.canvas.\(kind.rawValue)")
        let members = [CanvasDisplay(displayId: 11, x: 0, y: 0, width: 64, height: 48, scale: 1, rotation: .none),
                       CanvasDisplay(displayId: 12, x: 64, y: 0, width: 64, height: 48, scale: 1, rotation: .none)]
        let streams = Mutex<[UInt32: FakeStream]>([:])
        let opened = Mutex<[(UInt32, Int, Int)]>([])
        let canvas = HostCanvas(arrangement: CanvasArrangement(members: members), compositor: kind, makeStream: { display, configuration, _ in
            let stream = FakeStream()
            streams.withLock { $0[display] = stream }
            opened.withLock { $0.append((display, configuration.width, configuration.height)) }
            return stream
        }, observe: { display in
            guard let member = members.first(where: { $0.displayId == display }) else { return nil }
            return DisplayLayoutObservation(bounds: CGRect(x: member.x, y: member.y, width: member.width, height: member.height),
                                            scale: 1, rotation: 0, pixelWidth: 64, pixelHeight: 48)
        })
        let run = try AppleHostRun(config: config(target: .canvas(displayIds: [11, 12]), width: 128, height: 48),
                                   configuration: configuration(), queue: queue, output: discard(), canvas: canvas)
        let jobs = Mutex<[ObserverJob]>([])
        queue.sync { run.onJob = { job in jobs.withLock { $0.append(job) } } }
        queue.sync { _ = run.start() }
        let backend = try #require(run.canvasBackend)
        try await waitUntil { opened.withLock { $0.count } == 2 }
        #expect(opened.withLock { $0.allSatisfy { $0.1 == 64 && $0.2 == 48 } }, "members open at their tile size")
        for (index, value) in [UInt8(200), 90].enumerated() {
            nonisolated(unsafe) let image = try bgra(width: 64, height: 48, value: value)
            let now = MediaClock.now()
            queue.sync { backend.deliver(image, from: UInt64(index + 1), h0: now, h1: now) }
        }
        try await waitUntil { run.controller.stats().encoded >= 1 }
        let stats = run.controller.stats()
        #expect(stats.canvasComposed >= 1 && stats.canvasMemberFrames == 2)
        #expect(!jobs.withLock { $0 }.contains { if case .failed = $0 { true } else { false } })
        queue.sync { run.beginClose() }
        try await waitUntil { queue.sync { run.closed } != nil }
        #expect(streams.withLock { $0.values.allSatisfy { $0.stops.withLock { $0 } == 1 } }, "every member stopped once")
        #expect(run.surfaces.count == 0, "every member image and composite released at close")
    }
}
#endif
