// macOS only: the host side (a Mac captures and encodes).
#if os(macOS)
import CoreGraphics
import CoreMedia
import Foundation
import StreamCoreVideo
@preconcurrency import ScreenCaptureKit

/// StreamCore's `CaptureBackend` for a combined canvas (design 3.9): one ScreenCaptureKit
/// display stream per member, opened by StreamCore's executor (`openCapture` with a
/// `display` target at the member's tile size) and started, updated and stopped by it as
/// one group. Every member stream delivers on the stream queue, so each image drains
/// StreamCore inline; a member is named by its capture id (`member` is nil) and its images
/// go through the per-frame C ABI once StreamCore has its shape. Members are BGRA in the
/// display's own gamut on the Metal path (Display P3 where the display is wide gamut, else
/// sRGB; the compositor maps the gamut) and sRGB on the CPU path.
///
/// `observeLayout` reports a display's desktop bounds, scale and `CGDisplayRotation`, and a
/// display that is offline or mirrors another as gone (nil); StreamCore compares it with the
/// reviewed arrangement every 250 ms, so a rotation is a layout change. ScreenCaptureKit
/// composites a display as the desktop shows it, so member images arrive upright; should a
/// frame carry an orientation (`SCStreamFrameInfo.videoOrientation`, macOS 27), it becomes
/// the surface's `rotation` and the Metal compositor turns it. Only the stream queue calls into this object, except
/// `release` (a thread-safe leaf) and ScreenCaptureKit's stop delegate.
public final class CanvasCaptureBackend: NSObject, CaptureBackend, @unchecked Sendable {
    public typealias StreamFactory = (_ display: UInt32, _ configuration: SCStreamConfiguration,
                               _ member: Member) throws -> any ScreenCaptureStream
    public typealias Observe = @Sendable (UInt32) -> DisplayLayoutObservation?

    public let surfaces: SurfaceStore
    private let makeStream: StreamFactory
    private let observe: Observe
    private let wideGamut: Bool
    private let trace: (any FrameTrace)?
    private let lock = NSLock()
    private var events: CaptureEvents?
    private var members: [UInt64: Opened] = [:]
    private var nextID: UInt64 = 1
    /// The stream's port key for the C ABI's frames, set before the stream starts.
    public var nativePort: UInt64?
    /// The shape of each member's last full frame (stream queue only).
    private var shapes: [UInt64: (width: UInt32, height: UInt32, color: ColorTags, rotation: Rotation)] = [:]

    private struct Opened {
        let stream: any ScreenCaptureStream
        // periphery:ignore - the stream holds its output and delegate weakly; this keeps them
        let member: Member
        let configuration: SCStreamConfiguration
        let color: ColorTags
    }

    /// One member stream's output and delegate.
    public final class Member: NSObject, SCStreamOutput, SCStreamDelegate, @unchecked Sendable {
        let capture: UInt64
        weak var backend: CanvasCaptureBackend?
        init(capture: UInt64) { self.capture = capture }

        public func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
            guard type == .screen, sampleBuffer.isValid else { return }
            backend?.receive(sampleBuffer, from: capture)
        }

        public func stream(_ stream: SCStream, didStopWithError error: any Error) {
            backend?.stopped(capture)
        }
    }

    /// `wideGamut`: members keep a wide-gamut display's Display P3 (the Metal path); else
    /// every member is captured as sRGB.
    public init(surfaces: SurfaceStore, wideGamut: Bool, trace: (any FrameTrace)?,
         observe: @escaping Observe = { DisplayLayoutObservation.current($0) },
         makeStream: @escaping StreamFactory) {
        self.surfaces = surfaces; self.wideGamut = wideGamut; self.trace = trace
        self.observe = observe; self.makeStream = makeStream
    }

    /// The production member stream: a display filter on the stream queue.
    public static func screenStreams(content: SCShareableContent, queue: DispatchQueue) -> StreamFactory {
        { display, configuration, member in
            guard let target = content.displays.first(where: { $0.displayID == display }) else {
                throw MediaError.SourceGone
            }
            let stream = SCStream(filter: SCContentFilter(display: target, excludingWindows: []),
                                  configuration: configuration, delegate: member)
            try stream.addStreamOutput(member, type: .screen, sampleHandlerQueue: queue)
            return stream
        }
    }

    // MARK: CaptureBackend

    public func caps() -> CaptureCaps {
        CaptureCaps(targetKinds: [.display], windowFit: false, liveRegionMove: false,
                    cursorModes: [.hidden, .composited], surfaceKinds: [.ioSurface],
                    poolDepth: 5, virtualDisplays: false, regionInSurface: true)
    }

    public func enumerate() throws -> SourceCatalog { SourceCatalog(displays: [], windows: []) }

    public func observeLayout(display: UInt32) -> DisplayLayout? {
        // A mirrored or cloned display shows another's desktop: no layout of its own.
        guard let observed = observe(display), observed.mirrorOf == nil else { return nil }
        return DisplayLayout(displayId: display, pixelWidth: UInt32(clamping: observed.pixelWidth),
                             pixelHeight: UInt32(clamping: observed.pixelHeight), scale: observed.scale,
                             originX: Int32(clamping: Int(observed.bounds.minX.rounded())),
                             originY: Int32(clamping: Int(observed.bounds.minY.rounded())),
                             rotation: Self.rotation(degrees: observed.rotation))
    }

    public func openCapture(spec: CaptureSpec, queue: SerialQueue, events: CaptureEvents) throws -> UInt64 {
        guard case .display(let display) = spec.target else { throw MediaError.Unsupported }
        let wide = wideGamut && Self.isWideGamut(display)
        let configuration = SCStreamConfiguration()
        configuration.width = Int(spec.pixelWidth)
        configuration.height = Int(spec.pixelHeight)
        if spec.minIntervalNs > 0 {
            configuration.minimumFrameInterval = CMTime(value: CMTimeValue(spec.minIntervalNs), timescale: 1_000_000_000)
        }
        configuration.showsCursor = spec.cursor == .composited
        configuration.pixelFormat = kCVPixelFormatType_32BGRA
        configuration.colorSpaceName = wide ? CGColorSpace.displayP3 : CGColorSpace.sRGB
        configuration.backgroundColor = Self.black
        configuration.queueDepth = Int(spec.queueDepth)
        configuration.capturesAudio = false
        let id = lock.withLock { () -> UInt64 in defer { nextID += 1 }; return nextID }
        let member = Member(capture: id)
        member.backend = self
        let stream = try makeStream(display, configuration, member)
        let color = ColorTags(primaries: wide ? .displayP3 : .bt709, transfer: .srgb, matrix: .identity, fullRange: true)
        lock.withLock {
            self.events = events
            members[id] = Opened(stream: stream, member: member, configuration: configuration,
                                 color: color)
        }
        shapes[id] = nil
        return id
    }

    public func start(id: UInt64, op: OpToken) {
        guard let (stream, events) = opened(id) else { return }
        stream.startCapture { error in events.opDone(op: op, error: error.map { _ in MediaError.Internal }) }
    }

    public func update(id: UInt64, update: CaptureUpdate, op: OpToken) {
        guard let (stream, events) = opened(id) else { return }
        let configuration = lock.withLock { () -> SCStreamConfiguration? in
            guard let opened = members[id] else { return nil }
            if let interval = update.minIntervalNs {
                opened.configuration.minimumFrameInterval = CMTime(value: CMTimeValue(interval), timescale: 1_000_000_000)
            }
            if let cursor = update.cursor { opened.configuration.showsCursor = cursor == .composited }
            return opened.configuration
        }
        guard let configuration else { return }
        stream.updateConfiguration(configuration) { error in
            events.opDone(op: op, error: error.map { _ in MediaError.Internal })
        }
    }

    public func stop(id: UInt64, op: OpToken) {
        guard let (stream, events) = opened(id) else { return }
        stream.stopCapture { error in events.opDone(op: op, error: error.map { _ in MediaError.Internal }) }
    }

    public func release(surface: UInt64) { _ = surfaces.remove(surface) }

    /// `release` through the C ABI (the context is this backend, which the stream holds).
    public static let nativeRelease: StreamCoreNative.ReleaseFunction = { backend, surface in
        Unmanaged<CanvasCaptureBackend>.fromOpaque(backend!).takeUnretainedValue().release(surface: surface)
    }

    private func opened(_ id: UInt64) -> ((any ScreenCaptureStream), CaptureEvents)? {
        lock.withLock {
            guard let opened = members[id], let events else { return nil }
            return (opened.stream, events)
        }
    }

    // MARK: Member images (stream queue)

    private func receive(_ sample: CMSampleBuffer, from capture: UInt64) {
        let h1 = MediaClock.now()
        let info = ScreenCaptureFrame.info(sample)
        let status = (info?[SCStreamFrameInfo.status.rawValue] as? Int).flatMap(SCFrameStatus.init(rawValue:)) ?? .complete
        switch status {
        case .complete:
            guard let image = CMSampleBufferGetImageBuffer(sample) else { return }
            let h0 = (info?[SCStreamFrameInfo.displayTime.rawValue] as? NSNumber)
                .map { MediaClock.nanoseconds(machTicks: $0.uint64Value) } ?? 0
            deliver(image, from: capture, h0: h0, h1: h1, orientation: Self.orientation(of: info))
        case .blank, .suspended, .stopped:
            // StreamCore fails a started canvas whose member stops showing its picture.
            report(status == .blank ? .blank : status == .suspended ? .suspended : .stopped, from: capture)
        default:
            return // idle and started: the member keeps its last image
        }
    }

    /// A member's complete image, as its capture callback delivers it.
    public func deliver(_ image: CVPixelBuffer, from capture: UInt64, h0: UInt64, h1: UInt64, orientation: Rotation = .none) {
        guard let (events, requested) = lock.withLock({ () -> (CaptureEvents, ColorTags)? in
            guard let events, let opened = members[capture] else { return nil }
            return (events, opened.color)
        }) else { return }
        // ScreenCaptureKit's BGRA may carry only an ICC profile: tag what was asked for.
        if CVBufferCopyAttachment(image, kCVImageBufferColorPrimariesKey, nil) == nil {
            CanvasColor.attach(requested, to: image)
        }
        let id = surfaces.insert(image, h0: h0, h1: h1, rotation: orientation)
        trace?.count(.captured)
        let width = UInt32(CVPixelBufferGetWidth(image)), height = UInt32(CVPixelBufferGetHeight(image))
        let color = SurfaceColor.tags(of: image)
        if let port = nativePort, let shape = shapes[capture], shape.width == width, shape.height == height,
           shape.color == color, shape.rotation == orientation,
           StreamCoreNative.frame(port: port, capture: capture, surface: id, captureNs: h1, sourcePresentNs: h0) {
            return
        }
        shapes[capture] = (width, height, color, orientation)
        events.frame(id: capture, member: nil, surface: SurfaceInfo(
            id: id, kind: .ioSurface, format: .bgra, width: width, height: height, color: color, captureNs: h1,
            sourcePresentNs: h0 == 0 ? nil : h0, dirty: nil, rotation: orientation, fence: GpuFence.none), status: .complete)
    }

    /// A status without a new image.
    public func report(_ status: FrameStatus, from capture: UInt64) {
        guard let events = lock.withLock({ members[capture] == nil ? nil : events }) else { return }
        events.frame(id: capture, member: nil, surface: SurfaceInfo(
            id: 0, kind: .ioSurface, format: .bgra, width: 2, height: 2, color: ColorTags.srgb, captureNs: MediaClock.now(),
            sourcePresentNs: nil, dirty: nil, rotation: .none, fence: GpuFence.none), status: status)
    }

    fileprivate func stopped(_ capture: UInt64) {
        guard let events = lock.withLock({ members[capture] == nil ? nil : events }) else { return }
        events.sourceGone(id: capture, reason: .unavailable)
    }

    private static let black = CGColor(srgbRed: 0, green: 0, blue: 0, alpha: 1)

    /// `CGDisplayRotation`'s clockwise degrees as StreamCore's rotation.
    public static func rotation(degrees: Int) -> DisplayRotation {
        switch ((degrees % 360) + 360) % 360 {
        case 90: .rotate90
        case 180: .rotate180
        case 270: .rotate270
        default: .identity
        }
    }

    /// `SCStreamFrameInfo.videoOrientation` (macOS 27; not in the 26 SDK, so read through its
    /// exported key): a `CGImagePropertyOrientation`, as the quarter turn that shows the image
    /// upright. Mirrored orientations are not turned.
    private static let orientationKey: String? = {
        guard let symbol = dlsym(UnsafeMutableRawPointer(bitPattern: -2), "SCStreamFrameInfoVideoOrientation") else { return nil }
        return symbol.assumingMemoryBound(to: NSString.self).pointee as String
    }()

    public static func orientation(of info: NSDictionary?) -> Rotation {
        guard let key = orientationKey, let value = (info?[key] as? NSNumber)?.uint32Value else { return .none }
        return turn(CGImagePropertyOrientation(rawValue: value) ?? .up)
    }

    public static func turn(_ orientation: CGImagePropertyOrientation) -> Rotation {
        switch orientation {
        case .right: .rotate90
        case .down: .rotate180
        case .left: .rotate270
        default: .none
        }
    }

    /// A display whose colour space is wider than sRGB.
    public static func isWideGamut(_ display: UInt32) -> Bool {
        CGDisplayCopyColorSpace(display).isWideGamutRGB
    }
}

extension ColorTags {
    public static let srgb = ColorTags(primaries: .bt709, transfer: .srgb, matrix: .identity, fullRange: true)
}

/// One display's live global layout, read the same way discovery reports it.
/// Virtual displays are observed exactly like physical ones.
public struct DisplayLayoutObservation: Equatable, Sendable {
    public var bounds: CGRect
    public var scale: Double
    public var rotation: Int
    public var mirrorOf: UInt32?
    public var pixelWidth: Int
    public var pixelHeight: Int

    public init(bounds: CGRect, scale: Double, rotation: Int, mirrorOf: UInt32? = nil, pixelWidth: Int, pixelHeight: Int) {
        self.bounds = bounds; self.scale = scale; self.rotation = rotation; self.mirrorOf = mirrorOf
        self.pixelWidth = pixelWidth; self.pixelHeight = pixelHeight
    }

    public static func current(_ id: CGDirectDisplayID) -> DisplayLayoutObservation? {
        guard CGDisplayIsOnline(id) != 0 else { return nil }
        let bounds = CGDisplayBounds(id)
        var pixelWidth = Int(bounds.width), pixelHeight = Int(bounds.height), scale = 1.0
        if let mode = CGDisplayCopyDisplayMode(id) {
            pixelWidth = mode.pixelWidth
            pixelHeight = mode.pixelHeight
            if mode.width > 0 { scale = Double(mode.pixelWidth) / Double(mode.width) }
        }
        let mirror = CGDisplayMirrorsDisplay(id)
        return DisplayLayoutObservation(bounds: bounds, scale: scale, rotation: Int(CGDisplayRotation(id).rounded()),
            mirrorOf: mirror == kCGNullDirectDisplay ? nil : mirror, pixelWidth: pixelWidth, pixelHeight: pixelHeight)
    }
}
#endif
