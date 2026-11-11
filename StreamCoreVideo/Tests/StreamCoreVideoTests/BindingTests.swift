// The UniFFI binding smoke test (docs/STREAMCORE-VIDEO.md 3.8): an async foreign trait awaited
// from Rust, an async Rust method awaited from Swift, and one stream driven through Swift
// implementations of the backend traits, with callbacks answered synchronously (reentrancy) and
// from another queue.
import Foundation
import Testing
@testable import StreamCoreVideo

final class Probe: AsyncProbe {
    func bump(value: UInt64) async -> UInt64 {
        try? await Task.sleep(nanoseconds: 1_000_000)
        return value + 1
    }
}

@Test func asyncForeignTraitRoundTrip() async {
    #expect(await probeRoundTrip(probe: Probe(), start: 40, rounds: 2) == 42)
}

private let srgb = ColorTags(primaries: .bt709, transfer: .srgb, matrix: .bt709, fullRange: false)

final class Queue: SerialQueue, @unchecked Sendable {
    let queue = DispatchQueue(label: "streamcore.video.test.stream", qos: .userInteractive)
    let port: StreamPort
    init(port: StreamPort) { self.port = port }
    func wake() { queue.async { [port] in port.run() } }
    func wakeAfter(delayNs: UInt64, timer: UInt64) {
        queue.asyncAfter(deadline: .now() + .nanoseconds(Int(delayNs))) { [port] in port.runTimer(timer: timer) }
    }
}

final class Capture: CaptureBackend, @unchecked Sendable {
    private let lock = NSLock()
    private var events: CaptureEvents?
    private var nextSurface: UInt64 = 1
    private(set) var released: [UInt64] = []
    private(set) var stops = 0

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
    // Answered synchronously, from inside the outbound call: Rust posts it.
    func start(id: UInt64, op: OpToken) { lock.withLock { events }?.opDone(op: op, error: nil) }
    func update(id: UInt64, update: CaptureUpdate, op: OpToken) { lock.withLock { events }?.opDone(op: op, error: nil) }
    func stop(id: UInt64, op: OpToken) {
        let events = lock.withLock { stops += 1; return self.events }
        events?.opDone(op: op, error: nil)
    }
    func release(surface: UInt64) { lock.withLock { released.append(surface) } }

    /// A capture callback, delivered on the stream's queue.
    func emit(on queue: DispatchQueue) {
        queue.async { [self] in
            let (events, id) = lock.withLock { () -> (CaptureEvents?, UInt64) in
                defer { nextSurface += 1 }
                return (self.events, nextSurface)
            }
            events?.frame(id: 7, member: nil, surface: SurfaceInfo(
                id: id, kind: .ioSurface, format: .nv12, width: 64, height: 64, color: srgb,
                captureNs: 1_000 * id, sourcePresentNs: nil, dirty: nil, rotation: .none, fence: .none),
                status: .complete)
        }
    }
}

final class FakeEncoder: StreamEncoder, @unchecked Sendable {
    let spec: EncoderSpec
    let events: EncoderEvents
    private let output = DispatchQueue(label: "streamcore.video.test.encoder")
    init(spec: EncoderSpec, events: EncoderEvents) { self.spec = spec; self.events = events }
    func info() -> EncoderInfo { EncoderInfo(codec: spec.codec, name: "swift", hardware: false, lowLatency: true) }
    func submit(frame: UInt64, opts: SubmitOpts, permit: VideoWorkPermit) -> SubmitResult {
        // The output thread: bytes first, then the event, then the permit goes.
        output.async { [spec, events] in
            let payloadToken = opts.submitSeq * 2
            _ = spec.sink.submitEncoded(token: payloadToken, bytes: Data([0, 0, 0, 1, 0x65]))
            var configToken: UInt64?
            if opts.forceKeyframe {
                configToken = payloadToken + 1
                _ = spec.sink.submitEncoded(token: payloadToken + 1, bytes: Data("avcC".utf8))
            }
            events.encoded(frame: EncodedFrame(
                submitSeq: opts.submitSeq, ptsNs: opts.ptsNs, keyframe: opts.forceKeyframe, configToken: configToken,
                payloadToken: payloadToken, form: .avcc4, discardable: false, temporalId: nil, ltr: nil,
                timings: EncodeTimings(inputNs: nil, outputNs: nil)))
            withExtendedLifetime(permit) {}
        }
        return .submitted
    }
    func setRate(update: RateUpdate) throws {}
    func invalidate() { output.sync {} }
}

final class Encoders: EncoderFactory, @unchecked Sendable {
    func probe(adapter: UInt64?) -> [EncoderCaps] { [] }
    func create(spec: EncoderSpec, events: EncoderEvents) throws -> StreamEncoder {
        FakeEncoder(spec: spec, events: events)
    }
}

final class Out: VideoOut, @unchecked Sendable {
    private let lock = NSLock()
    private var revoked = false
    private(set) var packets: [VideoOutMeta] = []
    var count: Int { lock.withLock { packets.count } }
    func send(meta: VideoOutMeta, bytes: Data, receipt: SinkReceipt) {
        let accepted = lock.withLock { () -> Bool in
            guard !revoked else { return false }
            packets.append(meta)
            return true
        }
        receipt.complete(accepted: accepted)  // synchronous, with no lock held
    }
    func backpressured() -> Bool { false }
    func revoke() { lock.withLock { revoked = true } }
}

final class Observer: ObserverQueue, @unchecked Sendable {
    private let lock = NSLock()
    private(set) var jobs: [ObserverJob] = []
    func post(job: ObserverJob) { lock.withLock { jobs.append(job) } }
    var closed: CloseOutcome? {
        lock.withLock { jobs.compactMap { if case .closed(let outcome) = $0 { outcome } else { nil } }.first }
    }
}

private func waitUntil(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now + .seconds(5)
    while !condition() {
        guard ContinuousClock.now < deadline else { throw CancellationError() }
        try await Task.sleep(for: .milliseconds(5))
    }
}

@Test(.timeLimit(.minutes(1))) func aStreamRunsThroughSwiftAdaptersAndClosesAsync() async throws {
    let port = StreamPort()
    let queue = Queue(port: port)
    let capture = Capture(), out = Out(), observer = Observer()
    let spec = CaptureSpec(target: .display(displayId: 1), mode: .control, pixelWidth: 64, pixelHeight: 64,
                           minIntervalNs: 16_666_667, format: .nv12, color: .source, cursor: .composited, queueDepth: 5, adapterLuid: nil)
    let plan = StreamPlan(mode: .control, codec: .h264, encodeWidth: 64, encodeHeight: 64, fps: 60,
                          bitrateBps: 2_000_000, convert: .identity, encoderInput: .nv12, encoder: "swift",
                          colorOut: srgb, colorMapping: ColorMapping(allowP3: true, srgbTransferAsBt709: false),
                          extensions: [], permitTarget: 3, recovery: [.idr], onSourceGone: .fail, mediumCapBps: nil, adapterLuid: nil)
    let controller = try StreamController(parts: StreamParts(
        streamId: 1, spec: spec, plan: plan, port: port, queue: queue, capture: capture, converter: nil,
        encoders: Encoders(), out: out, observer: observer, timings: nil))
    _ = controller.start()
    try await waitUntil { controller.phase() == .running }
    for _ in 0..<3 {
        capture.emit(on: queue.queue)
        let before = out.count
        try await waitUntil { out.count > before }
    }
    let packets = out.packets
    #expect(packets.first?.config == true)
    #expect(packets.dropFirst().first?.keyframe == true)
    #expect(packets.filter { !$0.config }.map(\.header.sequence) == [1, 2, 3])

    #expect(await controller.close() == .closed)
    #expect(observer.closed == .closed)
    #expect(capture.stops == 1)
    #expect(Set(capture.released) == [1, 2, 3])
    #expect(controller.phase() == .closed)
}

@Test(.timeLimit(.minutes(1))) func invalidSettingsThrowBeforeAnythingNativeRuns() throws {
    let port = StreamPort()
    let spec = CaptureSpec(target: .display(displayId: 1), mode: .control, pixelWidth: 1, pixelHeight: 64,
                           minIntervalNs: 0, format: .nv12, color: .source, cursor: .composited, queueDepth: 5, adapterLuid: nil)
    let plan = StreamPlan(mode: .control, codec: .h264, encodeWidth: 64, encodeHeight: 64, fps: 60,
                          bitrateBps: 2_000_000, convert: .identity, encoderInput: .nv12, encoder: "swift",
                          colorOut: srgb, colorMapping: ColorMapping(allowP3: true, srgbTransferAsBt709: false),
                          extensions: [], permitTarget: 3, recovery: [.idr], onSourceGone: .fail, mediumCapBps: nil, adapterLuid: nil)
    #expect(throws: MediaError.self) {
        _ = try StreamController(parts: StreamParts(
            streamId: 1, spec: spec, plan: plan, port: port, queue: Queue(port: port), capture: Capture(),
            converter: nil, encoders: Encoders(), out: Out(), observer: Observer(), timings: nil))
    }
}
