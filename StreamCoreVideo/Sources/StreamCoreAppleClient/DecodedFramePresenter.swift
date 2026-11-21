import CoreMedia
import CoreVideo
import Foundation
import StreamCoreVideo

/// What the presenter tells its owner about each frame, for traces and feedback. Called
/// with the presenter's lock held: implementations only record.
public protocol PresentObserver: AnyObject, Sendable {
    /// `frame` was handed to the display (C4, the present request).
    func presentRequested(_ frame: DecompressionSession.Frame, at nanoseconds: UInt64)
    /// `frame` was replaced by a newer one before the display took it.
    func presentSuperseded(_ frame: DecompressionSession.Frame)
}

/// Puts decoded images on the stream's displays (each an `AVSampleBufferDisplayLayer`'s
/// renderer), wrapped as "display immediately" samples (design 5, 6.1). Responsive mode
/// hands each frame over as soon as it is decoded (R2); Smooth hands the newest over at
/// each `tick`. At most one decoded frame waits (design 6.2): a newer one replaces it
/// (R4), so if several are decoded before the display takes one, only the newest is shown.
///
/// One decode can show on several displays (`addDisplay`): the first is the primary,
/// whose readiness paces presentation; the others get the same frames when they can
/// take them.
///
/// Thread-safe; its own lock is never held while taking a caller's.
public final class DecodedFramePresenter: @unchecked Sendable {
    public private(set) var mode: PresentMode
    private weak var observer: (any PresentObserver)?
    private let lock = NSLock()
    private let queue = DispatchQueue(label: "streamcore.video.present", qos: .userInteractive)
    private var displays: [any VideoDisplay] = []
    private var pending: DecompressionSession.Frame?
    private var awaitingReady = false
    private var format: CMVideoFormatDescription?

    public init(mode: PresentMode, observer: (any PresentObserver)? = nil) {
        self.mode = mode
        self.observer = observer
    }

    /// Responsive or Smooth, from the next frame.
    public func setMode(_ mode: PresentMode) { lock.withLock { self.mode = mode } }

    /// The only display to present on, or nil; any waiting frame is dropped.
    public func setDisplay(_ display: (any VideoDisplay)?) {
        lock.withLock {
            displays = display.map { [$0] } ?? []
            pending = nil
            awaitingReady = false
        }
    }

    /// Another display showing the same frames (a mirror).
    public func addDisplay(_ display: any VideoDisplay) {
        lock.withLock { if !displays.contains(where: { $0 === display }) { displays.append(display) } }
    }

    public func removeDisplay(_ display: any VideoDisplay) {
        lock.withLock {
            let wasPrimary = displays.first.map { $0 === display } ?? false
            displays.removeAll { $0 === display }
            if wasPrimary { awaitingReady = false }
        }
    }

    /// The primary display's path, for diagnostics.
    public var path: VideoDisplays.Path? { lock.withLock { displays.first?.path } }

    /// A decoded frame (VideoToolbox's thread). Responsive mode presents it now.
    public func submit(_ frame: DecompressionSession.Frame) {
        lock.withLock {
            if let older = pending { observer?.presentSuperseded(older) }
            pending = frame
            if mode == .responsive { presentLocked() }
        }
    }

    /// A display refresh (main thread): Smooth mode presents the newest frame now.
    public func tick() {
        lock.withLock {
            guard mode == .smooth else { return }
            presentLocked()
        }
    }

    /// Drops the waiting frame and flushes every display.
    public func flush() {
        let displays = lock.withLock { () -> [any VideoDisplay] in
            pending = nil
            return self.displays
        }
        for display in displays { display.flush() }
    }

    private func presentLocked() {
        guard let primary = displays.first, let frame = pending else { return }
        if primary.isFailed { primary.flush() }
        guard primary.isReady else {
            // Keep the newest; the display calls back, so a still screen still shows its last change.
            if !awaitingReady {
                awaitingReady = true
                primary.whenReady(on: queue) { [weak self] in
                    guard let self else { return }
                    self.lock.withLock {
                        self.awaitingReady = false
                        self.presentLocked()
                    }
                }
            }
            return
        }
        pending = nil
        guard let sample = makeSample(frame) else { return }
        primary.enqueue(sample)
        observer?.presentRequested(frame, at: MediaClock.now())
        for mirror in displays.dropFirst() {
            if mirror.isFailed { mirror.flush() }
            // A mirror that has no room misses this frame; the next one replaces it anyway.
            if mirror.isReady, let copy = makeSample(frame) { mirror.enqueue(copy) }
        }
    }

    private func makeSample(_ frame: DecompressionSession.Frame) -> CMSampleBuffer? {
        if format.map({ !CMVideoFormatDescriptionMatchesImageBuffer($0, imageBuffer: frame.image) }) ?? true {
            var made: CMVideoFormatDescription?
            guard CMVideoFormatDescriptionCreateForImageBuffer(allocator: kCFAllocatorDefault,
                imageBuffer: frame.image, formatDescriptionOut: &made) == noErr else { return nil }
            format = made
        }
        guard let format else { return nil }
        var timing = CMSampleTimingInfo(duration: .invalid, presentationTimeStamp: frame.presentationTime,
                                        decodeTimeStamp: .invalid)
        var sample: CMSampleBuffer?
        guard CMSampleBufferCreateReadyWithImageBuffer(allocator: kCFAllocatorDefault, imageBuffer: frame.image,
            formatDescription: format, sampleTiming: &timing, sampleBufferOut: &sample) == noErr,
              let sample else { return nil }
        sample.markForImmediateDisplay(dependsOnOthers: false)
        return sample
    }
}
