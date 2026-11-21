import AVFoundation
import CoreMedia
import Foundation
import StreamCoreVideo
import Synchronization
import Testing
@testable import StreamCoreAppleClient

/// The port a client stream gives its decoder, recorded.
private final class Events: DecoderEvents, @unchecked Sendable {
    let decodedFrames = Mutex<[(seq: UInt64, c3: UInt64)]>([])
    let presentedFrames = Mutex<[(seq: UInt64, c5: UInt64, estimated: Bool)]>([])
    let inputReadyCount = Mutex(0)
    let failures = Mutex<[Bool]>([])
    init() { super.init(noHandle: NoHandle()) }
    required init(unsafeFromHandle handle: UInt64) { fatalError("a test double") }
    override func decoded(seq: UInt64, c3Ns: UInt64) { decodedFrames.withLock { $0.append((seq, c3Ns)) } }
    override func presented(seq: UInt64, c5Ns: UInt64, estimated: Bool) { presentedFrames.withLock { $0.append((seq, c5Ns, estimated)) } }
    override func inputReady() { inputReadyCount.withLock { $0 += 1 } }
    override func failed(error: MediaError, recoverable: Bool) { failures.withLock { $0.append(recoverable) } }
}

private final class CountingDisplay: VideoDisplay, @unchecked Sendable {
    let count = Mutex(0), flushes = Mutex(0)
    var path: VideoDisplays.Path { .layer }
    var isReady: Bool { true }
    var isFailed: Bool { false }
    func enqueue(_ sample: sending CMSampleBuffer) { count.withLock { $0 += 1 } }
    func flush() { flushes.withLock { $0 += 1 } }
    func whenReady(on queue: DispatchQueue, _ block: @escaping @Sendable () -> Void) { queue.async(execute: block) }
}

private func eventually(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now + .seconds(10)
    while !condition(), ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(5)) }
    try #require(condition())
}

private func meta(_ seq: UInt64, generation: UInt64 = 1, pts: UInt64 = 0, keyframe: Bool = true) -> SampleMeta {
    SampleMeta(seq: seq, generation: generation, ptsNs: pts, keyframe: keyframe, discardable: false, temporalId: 0,
               ltrToken: 0, ltrFlags: 0, layoutEpoch: nil)
}

private let bt709 = ColorTags(primaries: .bt709, transfer: .bt709, matrix: .bt709, fullRange: false)

/// `VideoToolboxStreamDecoder` driven as a client stream drives it, on iOS and macOS.
@Suite(.serialized) struct StreamDecoderTests {
    @Test(.timeLimit(.minutes(2)), arguments: [Codec.h264, .hevc])
    func itDecodesPresentsAndReportsEachFrame(codec: Codec) async throws {
        let encoded = try encodeTestStream(codec: codec, count: 6)
        let decoder = VideoToolboxStreamDecoder()
        let display = CountingDisplay()
        decoder.setDisplay(display)
        let events = Events()
        let port = StreamPort()
        let began = decoder.nowNs()
        let info = try decoder.configure(codec: codec, config: encoded.config, color: bt709,
                                         queue: ClientStreamQueue(port: port), events: events)
        #expect(info.lowLatencyConfirmed && info.maxQueued == 0)
        #expect(info.notes.contains { $0.contains("320x192") } && info.notes.contains("display: layer"))
        for (index, sample) in encoded.samples.enumerated() {
            let meta = meta(UInt64(index + 1), pts: UInt64(index) * 16_666_667, keyframe: sample.keyframe)
            var result = decoder.decode(sample: sample.data, meta: meta)
            // As a client stream does: after `Busy`, offer the sample again once there is room.
            let deadline = ContinuousClock.now + .seconds(10)
            while result == .busy, ContinuousClock.now < deadline {
                try await Task.sleep(for: .milliseconds(2))
                result = decoder.decode(sample: sample.data, meta: meta)
            }
            #expect(result == .accepted)
        }
        try await eventually { events.presentedFrames.withLock { $0.count } == 6 }
        let decoded = events.decodedFrames.withLock { $0 }, presented = events.presentedFrames.withLock { $0 }
        #expect(decoded.map(\.seq) == Array(1...6) && decoded.allSatisfy { $0.c3 >= began })
        // Presented after decode, and flagged as an estimate.
        #expect(presented.map(\.seq) == Array(1...6) && presented.allSatisfy(\.estimated))
        #expect(zip(decoded, presented).allSatisfy { $0.c3 <= $1.c5 })
        #expect(display.count.withLock { $0 } == 6 && events.failures.withLock { $0.isEmpty })

        // A flush forgets what waits and clears the display; a new configuration starts a new session.
        #expect(decoder.flush() == .completed)
        #expect(display.flushes.withLock { $0 } == 1)
        let again = Events()
        _ = try decoder.configure(codec: codec, config: encoded.config, color: bt709,
                                  queue: ClientStreamQueue(port: port), events: again)
        #expect(decoder.decode(sample: encoded.samples[0].data,
                               meta: meta(20, generation: 2)) == .accepted)
        try await eventually { again.decodedFrames.withLock { $0.map(\.seq) } == [20] }
        #expect(events.decodedFrames.withLock { $0.count } == 6, "the replaced session's port hears nothing more")
    }

    @Test func aBadConfigurationOrAFrameBeforeOneIsRefused() throws {
        let decoder = VideoToolboxStreamDecoder()
        #expect(decoder.decode(sample: Data([0, 0, 0, 1, 0x65]),
                               meta: meta(1)) == .failed(error: .Internal))
        #expect(throws: MediaError.InvalidSettings) {
            _ = try decoder.configure(codec: .h264, config: Data([1, 2, 3]), color: bt709,
                                      queue: ClientStreamQueue(port: StreamPort()), events: Events())
        }
        #expect(decoder.flush() == .completed)
        decoder.setMode(mode: .smooth)
        decoder.tick()
        #expect(decoder.displayPath == nil && decoder.presentTimesAreEstimated)
    }
}
