// macOS only: the host side (a Mac captures and encodes).
#if os(macOS)
import CoreMedia
import Foundation
import StreamCoreVideo
@preconcurrency import ScreenCaptureKit

/// The `SCStream` operations the backend uses (a seam for tests).
public protocol ScreenCaptureStream: AnyObject {
    func startCapture(completionHandler: (@Sendable ((any Error)?) -> Void)?)
    func updateConfiguration(_ streamConfig: SCStreamConfiguration, completionHandler: (@Sendable ((any Error)?) -> Void)?)
    func stopCapture(completionHandler: (@Sendable ((any Error)?) -> Void)?)
}

extension SCStream: ScreenCaptureStream {}

/// StreamCore's `CaptureBackend` over one ScreenCaptureKit stream (design 4.1): a
/// display, window or app target, NV12 `420v` or BGRA. The product builds the content
/// filter and the `SCStream` (what a target means, window fit and the settled size are
/// its own) and adds this object as the stream's output on the stream queue, so every
/// capture callback arrives on the executor's queue and drains inline (3.5).
///
/// `openCapture`, `start`, `update` and `stop` run on the stream queue during a drain;
/// their results are always posted through `opDone`. `release` is a thread-safe leaf.
/// Frames and releases go through StreamCore's per-frame C ABI once the run attached it
/// (`nativePort`, `nativeRelease`); a frame whose format, size or colour changed takes the
/// full UniFFI `frame`.
public final class ScreenCaptureBackend: NSObject, CaptureBackend, SCStreamOutput, @unchecked Sendable {
    public static let captureID: UInt64 = 1
    public let surfaces: SurfaceStore
    private let trace: (any FrameTrace)?
    private let meter: (any FrameMeter)?
    private let lock = NSLock()
    private var stream: (any ScreenCaptureStream)?
    private var configuration: SCStreamConfiguration
    private var events: CaptureEvents?
    /// The stream's port key for the C ABI's frames, set before the stream starts.
    public var nativePort: UInt64?
    /// The shape (format, size, colour) of the last frame handed over in full; only the
    /// stream queue touches it.
    private var shape: (format: PixelFormat, width: UInt32, height: UInt32, color: ColorTags)?

    /// The captured content's size in points (the display, window or app the filter
    /// shows): what a region is normalized to. Nil: the capture cannot move a region.
    private let contentSize: CGSize?
    /// ScreenCaptureKit began (true) or stopped (false) delivering suspended frames: the
    /// Mac is locked, at the login window, or the display is asleep. On the stream queue.
    public var onSuspended: ((Bool) -> Void)?
    private var suspended = false

    public init(configuration: SCStreamConfiguration, surfaces: SurfaceStore,
                trace: (any FrameTrace)?, meter: (any FrameMeter)?, contentSize: CGSize? = nil) {
        self.configuration = configuration
        self.surfaces = surfaces; self.trace = trace; self.meter = meter
        self.contentSize = contentSize.flatMap { $0.width > 0 && $0.height > 0 ? $0 : nil }
    }

    /// The stream this backend captures; set once before the controller starts.
    public func attach(_ stream: any ScreenCaptureStream) { lock.withLock { self.stream = stream } }

    // MARK: CaptureBackend

    public func caps() -> CaptureCaps {
        CaptureCaps(targetKinds: [.display, .window, .app], windowFit: true, liveRegionMove: contentSize != nil,
                    cursorModes: [.hidden, .composited], surfaceKinds: [.ioSurface],
                    poolDepth: UInt8(configuration.queueDepth), virtualDisplays: false, regionInSurface: true)
    }

    public func enumerate() throws -> SourceCatalog { SourceCatalog(displays: [], windows: []) }

    public func observeLayout(display: UInt32) -> DisplayLayout? { nil }

    public func openCapture(spec: CaptureSpec, queue: SerialQueue, events: CaptureEvents) throws -> UInt64 {
        try lock.withLock {
            guard stream != nil else { throw MediaError.Internal }
            self.events = events
        }
        // A new capture has no shape in StreamCore yet: its first frame goes in full. Only
        // the stream queue touches `shape`, and opens run there.
        shape = nil
        return Self.captureID
    }

    public func start(id: UInt64, op: OpToken) {
        let (stream, events) = lock.withLock { (self.stream, self.events) }
        guard let stream, let events else { return }
        measured {
            stream.startCapture { error in events.opDone(op: op, error: error.map { _ in MediaError.Internal }) }
        }
    }

    public func update(id: UInt64, update: CaptureUpdate, op: OpToken) {
        let (stream, events, configuration) = lock.withLock { () -> ((any ScreenCaptureStream)?, CaptureEvents?, SCStreamConfiguration) in
            if let interval = update.minIntervalNs {
                self.configuration.minimumFrameInterval = CMTime(value: CMTimeValue(interval), timescale: 1_000_000_000)
            }
            if let cursor = update.cursor { self.configuration.showsCursor = cursor == .composited }
            if let region = update.region, let contentSize {
                // A live move: ScreenCaptureKit crops the same stream, no restart.
                self.configuration.sourceRect = CGRect(x: region.x * contentSize.width, y: region.y * contentSize.height,
                                                       width: region.width * contentSize.width,
                                                       height: region.height * contentSize.height)
            }
            return (self.stream, self.events, self.configuration)
        }
        guard let stream, let events else { return }
        if update.region != nil, contentSize == nil {
            events.opDone(op: op, error: MediaError.Unsupported)
            return
        }
        measured {
            stream.updateConfiguration(configuration) { error in
                events.opDone(op: op, error: error.map { _ in MediaError.Internal })
            }
        }
    }

    public func stop(id: UInt64, op: OpToken) {
        let (stream, events) = lock.withLock { (self.stream, self.events) }
        guard let stream, let events else { return }
        measured {
            stream.stopCapture { error in events.opDone(op: op, error: error.map { _ in MediaError.Internal }) }
        }
    }

    /// Native work Rust called (counted out of the FFI overhead when measured).
    private func measured(_ body: () -> Void) {
        if let meter { meter.native(sequence: nil, sent: false, body) } else { body() }
    }

    public func release(surface: UInt64) { measured { forget(surface) } }

    /// `release` through the C ABI (the context is this backend, which the stream holds).
    public static let nativeRelease: StreamCoreNative.ReleaseFunction = { backend, surface in
        Unmanaged<ScreenCaptureBackend>.fromOpaque(backend!).takeUnretainedValue().release(surface: surface)
    }

    private func forget(_ surface: UInt64) {
        guard let entry = surfaces.remove(surface) else { return }
        // Skip (b): replaced in the latest slot (or the mailbox) before submit.
        if !entry.submitted { trace?.replaced(h0: entry.h0, h1: entry.h1) }
    }

    // MARK: SCStreamOutput (on the stream queue)

    public func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen, sampleBuffer.isValid else { return }
        let (current, events) = lock.withLock { (self.stream, self.events) }
        guard stream === current, let events, let image = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        let h1 = MediaClock.now()
        let info = ScreenCaptureFrame.info(sampleBuffer)
        let status = (info?[SCStreamFrameInfo.status.rawValue] as? Int).flatMap(SCFrameStatus.init(rawValue:))
        if let status, status != .complete {
            // Idle and blank frames carry no new image: the latest one stays.
            trace?.count(status == .idle ? .statusIdle : .statusOther)
            self.status(status)
            return
        }
        trace?.count(.captured)
        let h0 = (info?[SCStreamFrameInfo.displayTime.rawValue] as? NSNumber)
            .map { MediaClock.nanoseconds(machTicks: $0.uint64Value) } ?? 0
        deliver(image, h0: h0, h1: h1, to: events)
    }

    /// A frame status without a new image, on the stream queue: suspension is reported
    /// once when it begins.
    func status(_ status: SCFrameStatus) {
        guard status == .suspended, !suspended else { return }
        suspended = true
        onSuspended?(true)
    }

    /// A complete image, as a capture callback delivers it: register it and hand it to
    /// Rust, which may drain inline (admission and submit) on this thread.
    public func deliver(_ image: CVPixelBuffer, h0: UInt64, h1: UInt64, to events: CaptureEvents? = nil) {
        guard let events = events ?? lock.withLock({ self.events }) else { return }
        if suspended {
            suspended = false
            onSuspended?(false)
        }
        let id = surfaces.insert(image, h0: h0, h1: h1)
        let pixelFormat = CVPixelBufferGetPixelFormatType(image)
        let format: PixelFormat = pixelFormat == kCVPixelFormatType_32BGRA ? .bgra : .nv12
        let width = UInt32(CVPixelBufferGetWidth(image)), height = UInt32(CVPixelBufferGetHeight(image))
        let color = SurfaceColor.tags(of: image)
        if let port = nativePort, let shape, shape.format == format, shape.width == width, shape.height == height,
           shape.color == color {
            // The same shape as the last full frame: the per-frame C call, primitives only.
            let taken = if let meter {
                meter.rust(drain: false) { StreamCoreNative.frame(port: port, capture: Self.captureID, surface: id, captureNs: h1, sourcePresentNs: h0) }
            } else {
                StreamCoreNative.frame(port: port, capture: Self.captureID, surface: id, captureNs: h1, sourcePresentNs: h0)
            }
            if taken { return }
            // Not taken (no shape for this capture yet, or the stream is gone): deliver it in
            // full, which also releases it when the stream is gone.
        }
        shape = (format, width, height, color)
        let surface = SurfaceInfo(id: id, kind: .ioSurface, format: format, width: width, height: height,
            color: color, captureNs: h1, sourcePresentNs: h0 == 0 ? nil : h0,
            dirty: nil, rotation: .none, fence: GpuFence.none)
        if let meter {
            meter.rust(drain: false) { events.frame(id: Self.captureID, member: nil, surface: surface, status: .complete) }
        } else {
            events.frame(id: Self.captureID, member: nil, surface: surface, status: .complete)
        }
    }
}

/// A captured sample's frame info (`SCStreamFrameInfo` keys).
public enum ScreenCaptureFrame {
    public static func info(_ sampleBuffer: CMSampleBuffer) -> NSDictionary? {
        guard let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: false),
              CFArrayGetCount(attachments) > 0 else { return nil }
        return unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFDictionary.self) as NSDictionary
    }
}
#endif
