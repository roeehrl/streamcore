import AVFoundation
import CoreMedia
import ObjectiveC

/// Native display boundary. A caller serializes its enqueues (`DecodedFramePresenter`
/// does under its own lock); a display reports pressure before another sample is
/// allocated.
public protocol VideoDisplay: AnyObject, Sendable {
    /// Which path this display uses.
    var path: VideoDisplays.Path { get }
    var isReady: Bool { get }
    var isFailed: Bool { get }
    func enqueue(_ sample: sending CMSampleBuffer)
    func flush()
    /// Calls `block` once on `queue` when the display can take another sample.
    func whenReady(on queue: DispatchQueue, _ block: @escaping @Sendable () -> Void)
}

public enum VideoDisplays {
    /// Posted (object: the display) when a display's renderer fails or needs a flush to
    /// resume decoding, as after returning from the background.
    public static let failedNotification = Notification.Name("StreamCoreVideoDisplayFailed")

    /// Which path a display uses, for diagnostics.
    public enum Path: String, Sendable {
        /// The renderer behind a render synchronizer's receiver (iOS and macOS 27).
        case receiver
        /// The layer's renderer and its own queue (earlier systems).
        case layer
    }

    /// The display for `layer`: its renderer behind a render synchronizer's receiver, or
    /// before iOS and macOS 27 the renderer's own queue.
    @MainActor
    public static func display(for layer: AVSampleBufferDisplayLayer) -> any VideoDisplay {
        guard #available(iOS 27.0, macOS 27.0, *) else { return LayerVideoDisplay(layer.sampleBufferRenderer) }
        return ReceiverVideoDisplay.shared(for: layer)
    }
}

/// An `AVSampleBufferDisplayLayer`'s renderer added to its own
/// `AVSampleBufferRenderSynchronizer`, fed through the receiver's async `enqueue(_:)`.
///
/// One per layer, kept by the layer (a renderer joins one synchronizer), so every stream
/// attached to that layer shares it as it shared the renderer. The synchronizer stays at
/// rate 0: samples are marked "display immediately" (R2), so they show on arrival
/// regardless of time, and the renderer's `displayedPixelBuffer()` (C5, the loupe) keeps
/// working, which it does not while the rate is non-zero.
///
/// At most `DecompressionSession.queueBound` samples wait for the renderer (R4); a
/// display-immediately sample replaces anything queued before it, so the newest wins.
@available(iOS 27.0, macOS 27.0, *)
public final class ReceiverVideoDisplay: VideoDisplay, @unchecked Sendable {
    private let feed: SampleBufferFeed
    private let events: Task<Void, Never>

    private nonisolated(unsafe) static var layerKey: UInt8 = 0

    @MainActor
    public static func shared(for layer: AVSampleBufferDisplayLayer) -> ReceiverVideoDisplay {
        if let display = objc_getAssociatedObject(layer, &layerKey) as? ReceiverVideoDisplay { return display }
        let display = ReceiverVideoDisplay(layer.sampleBufferRenderer)
        objc_setAssociatedObject(layer, &layerKey, display, .OBJC_ASSOCIATION_RETAIN_NONATOMIC)
        return display
    }

    private init(_ renderer: AVSampleBufferVideoRenderer) {
        let synchronizer = AVSampleBufferRenderSynchronizer()
        let port = Port(synchronizer.sampleBufferReceiver(adding: renderer), synchronizer: synchronizer)
        let relay = FailureRelay()
        let feed = SampleBufferFeed(
            capacity: DecompressionSession.queueBound,
            port: SampleBufferFeed.Port(enqueue: { await port.enqueue($0, epoch: $1) },
                                        flush: { await port.flush(through: $0) }),
            onFailure: { relay.post() })
        self.feed = feed
        events = Task.detached(priority: .high) { [weak feed] in
            for await event in await port.events {
                switch event {
                case .requiresFlushToResumeDecoding, .failed: feed?.markFailed()
                // A decode error alone leaves the renderer running; the chain recovers at
                // the next keyframe, which the continuity check already asks for.
                case .didFailToDecode: break
                @unknown default: feed?.markFailed()
                }
            }
        }
        relay.display = self
    }

    deinit { events.cancel() }

    public var path: VideoDisplays.Path { .receiver }
    public var isReady: Bool { feed.isReady }
    public var isFailed: Bool { feed.isFailed }
    public func enqueue(_ sample: sending CMSampleBuffer) { feed.enqueue(sample) }
    public func flush() { feed.flush() }
    public func whenReady(on queue: DispatchQueue, _ block: @escaping @Sendable () -> Void) {
        feed.whenReady(on: queue, block)
    }

    /// Posts a failure naming this display, which exists only after the feed.
    private final class FailureRelay: @unchecked Sendable {
        weak var display: ReceiverVideoDisplay?
        func post() {
            NotificationCenter.default.post(name: VideoDisplays.failedNotification, object: display)
        }
    }

    /// The receiver, confined to this actor. Its async `enqueue` suspends here, so a
    /// `flush` can still run meanwhile and cancel it.
    ///
    /// The receiver is stored type-erased: the runtime realizes this actor's class at launch
    /// on every system, and a stored property naming the iOS and macOS 27 receiver type would
    /// crash it on an older one (see `SampleBufferFeed.Sample`). Only code that runs on 27
    /// casts it back.
    private actor Port {
        private let stored: Any
        // periphery:ignore - keeps the receiver's synchronizer alive as long as the receiver
        private let synchronizer: AVSampleBufferRenderSynchronizer
        private var flushed: UInt64 = 0

        init(_ receiver: sending AVSampleBufferVideoRenderer.Receiver, synchronizer: AVSampleBufferRenderSynchronizer) {
            stored = receiver
            self.synchronizer = synchronizer
        }

        private var receiver: AVSampleBufferVideoRenderer.Receiver {
            // swiftlint:disable:next force_cast
            stored as! AVSampleBufferVideoRenderer.Receiver
        }

        var events: some Sendable & AsyncSequence<AVSampleBufferVideoRenderer.Receiver.RenderingEvent, Never> {
            receiver.renderingEventsAfterFinishedEnqueuing
        }

        func flush(through epoch: UInt64) {
            guard epoch > flushed else { return }
            flushed = epoch
            receiver.flush()
        }

        func enqueue(_ sample: SampleBufferFeed.Sample, epoch: UInt64) async -> SampleBufferFeed.Outcome {
            flush(through: epoch)
            guard epoch == flushed else { return .taken }  // flushed since: drop it
            let ready = CMReadySampleBuffer<CMSampleBuffer.DynamicContent>(unsafeBuffer: sample.buffer)
            do {
                switch try await receiver.enqueue(ready) {
                case .enqueued, .enqueuedWithDecodeFailures, .cancelledDueToFlush: return .taken
                case .cancelledDueToFlushRequiredToResume, .cancelledDueToError: return .failed
                @unknown default: return .failed
                }
            } catch {
                return .failed
            }
        }
    }
}

/// Before iOS and macOS 27: an `AVSampleBufferDisplayLayer`'s renderer, which (unlike the
/// main-actor layer) accepts calls from any thread. Weak: the layer owns it.
public final class LayerVideoDisplay: VideoDisplay, @unchecked Sendable {
    private weak var renderer: AVSampleBufferVideoRenderer?
    private var observers: [NSObjectProtocol] = []

    public init(_ renderer: AVSampleBufferVideoRenderer) {
        self.renderer = renderer
        // Returning from background invalidates the decode session: the layer needs a
        // flush and a fresh keyframe or the video freezes though packets keep arriving.
        for name in [AVSampleBufferVideoRenderer.requiresFlushToResumeDecodingDidChangeNotification,
                     AVSampleBufferVideoRenderer.didFailToDecodeNotification] {
            observers.append(NotificationCenter.default.addObserver(forName: name, object: renderer, queue: nil) {
                [weak self] _ in
                NotificationCenter.default.post(name: VideoDisplays.failedNotification, object: self)
            })
        }
    }

    deinit { for observer in observers { NotificationCenter.default.removeObserver(observer) } }

    public var path: VideoDisplays.Path { .layer }
    public var isReady: Bool { renderer?.isReadyForMoreMediaData == true }
    public var isFailed: Bool { renderer.map { $0.status == .failed || $0.requiresFlushToResumeDecoding } ?? false }
    public func enqueue(_ sample: sending CMSampleBuffer) { renderer?.enqueue(sample) }
    public func flush() { renderer?.flush() }
    public func whenReady(on queue: DispatchQueue, _ block: @escaping @Sendable () -> Void) {
        guard let renderer else { return }
        renderer.requestMediaDataWhenReady(on: queue) { [weak self] in
            self?.renderer?.stopRequestingMediaData()
            block()
        }
    }
}
