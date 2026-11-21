import AVFoundation
import CoreMedia
import CoreVideo
import Foundation
import Synchronization
import Testing
@testable import StreamCoreAppleClient

/// A one-colour 32×32 image as a "display immediately" sample.
private func imageSample(_ value: UInt8) throws -> CMSampleBuffer {
    var output: CVPixelBuffer?
    try #require(CVPixelBufferCreate(kCFAllocatorDefault, 32, 32, kCVPixelFormatType_32BGRA,
        [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &output) == kCVReturnSuccess)
    let buffer = try #require(output)
    CVPixelBufferLockBaseAddress(buffer, [])
    memset(CVPixelBufferGetBaseAddress(buffer), Int32(value), CVPixelBufferGetDataSize(buffer))
    CVPixelBufferUnlockBaseAddress(buffer, [])
    var format: CMVideoFormatDescription?
    try #require(CMVideoFormatDescriptionCreateForImageBuffer(allocator: kCFAllocatorDefault, imageBuffer: buffer,
                                                             formatDescriptionOut: &format) == noErr)
    var timing = CMSampleTimingInfo(duration: .invalid, presentationTimeStamp: .zero, decodeTimeStamp: .invalid)
    var sample: CMSampleBuffer?
    try #require(CMSampleBufferCreateReadyWithImageBuffer(allocator: kCFAllocatorDefault, imageBuffer: buffer,
        formatDescription: try #require(format), sampleTiming: &timing, sampleBufferOut: &sample) == noErr)
    let made = try #require(sample)
    made.markForImmediateDisplay(dependsOnOthers: false)
    return made
}

private func eventually(isolation: isolated (any Actor)? = #isolation, _ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now + .seconds(30)
    while !condition(), ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(5)) }
    try #require(condition())
}

/// A receiver that holds each enqueue until the test releases it.
@available(iOS 27.0, macOS 27.0, *)
private final class GatedPort: Sendable {
    private struct State {
        var enqueued: [UInt64] = []
        var flushed: [UInt64] = []
        var held: [CheckedContinuation<SampleBufferFeed.Outcome, Never>] = []
    }
    private let state = Mutex(State())

    var port: SampleBufferFeed.Port {
        SampleBufferFeed.Port(
            enqueue: { _, epoch in
                await withCheckedContinuation { held in
                    self.state.withLock { $0.enqueued.append(epoch); $0.held.append(held) }
                }
            },
            flush: { epoch in self.state.withLock { $0.flushed.append(epoch) } })
    }
    var enqueued: [UInt64] { state.withLock { $0.enqueued } }
    var flushed: [UInt64] { state.withLock { $0.flushed } }
    var holding: Int { state.withLock { $0.held.count } }

    func release(_ outcome: SampleBufferFeed.Outcome = .taken) {
        state.withLock { $0.held.isEmpty ? nil : $0.held.removeFirst() }?.resume(returning: outcome)
    }
}

struct SampleBufferFeedTests {
    @Test func backlogStopsAtCapacityAndReadinessReturnsWhenTheReceiverTakesOne() async throws {
        guard #available(iOS 27.0, macOS 27.0, *) else { return }
        let gate = GatedPort()
        let feed = SampleBufferFeed(capacity: 2, port: gate.port, onFailure: {})
        #expect(feed.isReady)
        feed.enqueue(try imageSample(1))
        #expect(feed.isReady)
        feed.enqueue(try imageSample(2))
        #expect(!feed.isReady)
        let ready = Mutex(0)
        feed.whenReady(on: .global()) { ready.withLock { $0 += 1 } }
        try await eventually { gate.holding == 1 }
        #expect(ready.withLock { $0 } == 0)
        gate.release()
        try await eventually { ready.withLock { $0 } == 1 && feed.isReady }
        try await eventually { gate.holding == 1 }  // the second sample, in order
        gate.release()
        #expect(gate.enqueued == [0, 0])
    }

    @Test func flushDropsTheBacklogAtOnceAndFlushesBeforeLaterSamples() async throws {
        guard #available(iOS 27.0, macOS 27.0, *) else { return }
        let gate = GatedPort()
        let feed = SampleBufferFeed(capacity: 2, port: gate.port, onFailure: {})
        feed.enqueue(try imageSample(1))
        feed.enqueue(try imageSample(2))
        try await eventually { gate.holding == 1 }
        let ready = Mutex(false)
        feed.whenReady(on: .global()) { ready.withLock { $0 = true } }
        feed.flush()
        #expect(feed.isReady)
        try await eventually { ready.withLock { $0 } && gate.flushed == [1] }
        // The enqueue that was waiting completes (cancelled by the flush) without
        // counting against the new backlog.
        feed.enqueue(try imageSample(3))
        gate.release(.taken)
        #expect(feed.isReady)
        // The stale sample still reaches the port, tagged with its old epoch for it to drop.
        try await eventually { gate.holding == 1 }
        gate.release()
        try await eventually { gate.holding == 1 }
        gate.release()
        #expect(gate.enqueued == [0, 0, 1])
    }

    @Test func failureIsReportedOnceAndClearedByFlush() async throws {
        guard #available(iOS 27.0, macOS 27.0, *) else { return }
        let gate = GatedPort()
        let failures = Mutex(0)
        let feed = SampleBufferFeed(capacity: 2, port: gate.port, onFailure: { failures.withLock { $0 += 1 } })
        feed.enqueue(try imageSample(1))
        try await eventually { gate.holding == 1 }
        gate.release(.failed)
        try await eventually { feed.isFailed }
        #expect(!feed.isReady)
        feed.markFailed()  // a rendering event for the same failure
        #expect(failures.withLock { $0 } == 1)
        feed.flush()
        #expect(!feed.isFailed && feed.isReady)
        feed.markFailed()
        #expect(failures.withLock { $0 } == 2)
    }
}
