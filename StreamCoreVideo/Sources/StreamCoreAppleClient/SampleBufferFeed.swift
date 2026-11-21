import CoreMedia
import Foundation

/// Hands sample buffers from synchronous code to an AVFoundation sample-buffer receiver
/// (`AVSampleBufferRenderSynchronizer.sampleBufferReceiver(adding:)`, iOS and macOS 27).
///
/// One task takes the samples in submission order and awaits the receiver's async
/// `enqueue(_:)`, which suspends while the renderer has no room. The samples handed over
/// and not yet taken are the backlog; `isReady` is false once it reaches `capacity`, which
/// replaces `isReadyForMoreMediaData`, and `whenReady` replaces `requestMediaDataWhenReady`.
/// `flush()` drops the backlog at once and flushes the receiver before anything enqueued
/// after it, also cancelling an enqueue that is waiting for room.
///
/// Thread-safe. The receiver itself is confined to its `Port` (an actor).
@available(iOS 27.0, macOS 27.0, *)
public final class SampleBufferFeed: @unchecked Sendable {
    public typealias Sample = CMReadySampleBuffer<CMSampleBuffer.DynamicContent>

    /// What the receiver made of one sample.
    public enum Outcome: Sendable {
        /// Taken, or cancelled by a flush.
        case taken
        /// The receiver needs a flush before it takes more.
        case failed
    }

    /// The receiver's side. `flush(epoch)` flushes it unless that epoch is already flushed;
    /// `enqueue` first catches up with the sample's epoch, so ordering never depends on
    /// which task runs first.
    public struct Port: Sendable {
        public let enqueue: @Sendable (Sample, _ epoch: UInt64) async -> Outcome
        public let flush: @Sendable (_ epoch: UInt64) async -> Void
        public init(enqueue: @escaping @Sendable (Sample, _ epoch: UInt64) async -> Outcome,
                    flush: @escaping @Sendable (_ epoch: UInt64) async -> Void) {
            self.enqueue = enqueue; self.flush = flush
        }
    }

    private struct Item: Sendable {
        let sample: Sample
        let epoch: UInt64
    }

    public let capacity: Int
    private let port: Port
    private let onFailure: @Sendable () -> Void
    private let lock = NSLock()
    private var backlog = 0
    private var epoch: UInt64 = 0
    private var failed = false
    private var waiters: [(queue: DispatchQueue, block: @Sendable () -> Void)] = []
    private let continuation: AsyncStream<Item>.Continuation

    /// `onFailure` runs (on an arbitrary thread, no lock held) when the receiver fails.
    public init(capacity: Int, port: Port, onFailure: @escaping @Sendable () -> Void) {
        self.capacity = capacity
        self.port = port
        self.onFailure = onFailure
        let (items, continuation) = AsyncStream.makeStream(of: Item.self)
        self.continuation = continuation
        Task.detached(priority: .high) { [weak self] in
            for await item in items {
                let outcome = await port.enqueue(item.sample, item.epoch)
                self?.taken(epoch: item.epoch, outcome)
            }
        }
    }

    deinit { continuation.finish() }

    public var isReady: Bool { lock.withLock { !failed && backlog < capacity } }
    public var isFailed: Bool { lock.withLock { failed } }

    /// Queues `sample` behind the backlog; callers check `isReady` first.
    public func enqueue(_ sample: sending CMSampleBuffer) {
        let ready = Sample(unsafeBuffer: sample)
        lock.withLock {
            backlog += 1
            continuation.yield(Item(sample: ready, epoch: epoch))
        }
    }

    public func flush() {
        let (flushed, ready) = lock.withLock {
            epoch += 1
            backlog = 0
            failed = false
            return (epoch, takeWaitersLocked())
        }
        Task.detached(priority: .high) { [port] in await port.flush(flushed) }
        Self.run(ready)
    }

    /// Calls `block` once on `queue` when the feed can take another sample.
    public func whenReady(on queue: DispatchQueue, _ block: @escaping @Sendable () -> Void) {
        let now = lock.withLock { () -> Bool in
            if !failed && backlog < capacity { return true }
            waiters.append((queue, block))
            return false
        }
        if now { queue.async(execute: block) }
    }

    /// A rendering event reported that the receiver failed.
    public func markFailed() {
        if lock.withLock({ failLocked() }) { onFailure() }
    }

    private func taken(epoch taken: UInt64, _ outcome: Outcome) {
        var newlyFailed = false
        let ready = lock.withLock { () -> [(queue: DispatchQueue, block: @Sendable () -> Void)] in
            guard taken == epoch else { return [] }  // flushed meanwhile: already counted out
            backlog -= 1
            if outcome == .failed { newlyFailed = failLocked() }
            return failed || backlog >= capacity ? [] : takeWaitersLocked()
        }
        if newlyFailed { onFailure() }
        Self.run(ready)
    }

    /// Sets the failure flag; true if it was clear (the owner hears once per failure).
    private func failLocked() -> Bool {
        defer { failed = true }
        return !failed
    }

    private func takeWaitersLocked() -> [(queue: DispatchQueue, block: @Sendable () -> Void)] {
        defer { waiters.removeAll() }
        return waiters
    }

    private static func run(_ waiters: [(queue: DispatchQueue, block: @Sendable () -> Void)]) {
        for waiter in waiters { waiter.queue.async(execute: waiter.block) }
    }
}
