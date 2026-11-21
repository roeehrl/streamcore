import AVFoundation
import CoreMedia
import Foundation
import StreamCoreVideo
import VideoToolbox

/// StreamCore's `StreamDecoder` over VideoToolbox (design 5): the decoder behind a
/// `ClientStream` on iOS and macOS. Each `configure` starts a `DecompressionSession` for
/// the generation's `avcC` or `hvcC`; decoded images go to a `DecodedFramePresenter` and
/// onto the displays attached here. `ClientStream` does the rest: the queue bound,
/// flushes, keyframe requests, LTR acknowledgements and `videoFeedback`.
///
/// C3 (`decoded`) is VideoToolbox's output time, exact. C5 (`presented`) is the moment
/// the frame was handed to the display, reported as estimated: AVFoundation gives no
/// per-frame present time (`presentTimesAreEstimated`).
///
/// Attach a display with `setDisplay` (from `VideoDisplays.display(for:)`), and more with
/// `addDisplay` to show the same decode twice. `StreamDecoder`'s calls come from the
/// stream's queue; the display calls are safe from any thread.
public final class VideoToolboxStreamDecoder: StreamDecoder, PresentObserver, @unchecked Sendable {
    private let presenter: DecodedFramePresenter
    private let log: (any MediaLog)?
    private let lock = NSLock()
    private var current: Running?
    /// A `decode` returned `Busy`: tell Rust when the session can take input again.
    private var inputWanted = false

    /// One session and the port its outputs go to. Outputs of a replaced session still
    /// post through their own port, which Rust ignores.
    private final class Running: @unchecked Sendable {
        let format: CMVideoFormatDescription
        let events: DecoderEvents
        var session: DecompressionSession?
        init(format: CMVideoFormatDescription, events: DecoderEvents) { self.format = format; self.events = events }
    }

    public init(mode: PresentMode = .responsive, log: (any MediaLog)? = nil) {
        presenter = DecodedFramePresenter(mode: mode)
        self.log = log
        presenter.setObserver(self)
    }

    // MARK: Displays

    /// The only display to present on, or nil.
    public func setDisplay(_ display: (any VideoDisplay)?) { presenter.setDisplay(display) }
    /// Another display showing the same frames (a mirror): one decode, several layers.
    public func addDisplay(_ display: any VideoDisplay) { presenter.addDisplay(display) }
    public func removeDisplay(_ display: any VideoDisplay) { presenter.removeDisplay(display) }
    /// A display refresh: Smooth mode presents the newest frame now.
    public func tick() { presenter.tick() }

    /// The primary display's path (the receiver on iOS and macOS 27, else the layer's
    /// renderer), for diagnostics.
    public var displayPath: VideoDisplays.Path? { presenter.path }
    /// C5 is always an estimate on Apple platforms: the time of the present request.
    public var presentTimesAreEstimated: Bool { true }

    // MARK: StreamDecoder

    public func configure(codec: Codec, config: Data, color: ColorTags, queue: SerialQueue,
                          events: DecoderEvents) throws -> DecoderInfo {
        let format: CMVideoFormatDescription
        do { format = try VideoFormat.description(codec: codec, config: config) } catch VideoFormat.FormatError.unsupportedCodec {
            throw MediaError.Unsupported
        } catch {
            log?.error("StreamCore client: codec configuration refused (\(error))")
            throw MediaError.InvalidSettings
        }
        let running = Running(format: format, events: events)
        let session: DecompressionSession
        do {
            session = try DecompressionSession(format: format, fallbackColor: color) { [weak self, weak running] _, output in
                guard let self, let running else { return }
                self.handle(output, of: running)
            }
        } catch let failure as DecompressionSession.CreateFailed {
            log?.error("StreamCore client: VTDecompressionSessionCreate failed (\(failure.status))")
            // Not Unsupported: the stream waits for the host to repeat the configuration.
            throw MediaError.EncoderFailed(recoverable: true)
        }
        running.session = session
        let old = lock.withLock { () -> Running? in
            defer { current = running; inputWanted = false }
            return current
        }
        // Outputs still to come from the old session are dropped; never on VideoToolbox's thread.
        old?.session?.invalidate()
        let hardware = Self.usesHardware(session.native)
        let dimensions = CMVideoFormatDescriptionGetDimensions(format)
        return DecoderInfo(lowLatencyConfirmed: true, hardware: hardware, maxQueued: 0,
                           notes: ["VideoToolbox \(codec == .hevc ? "HEVC" : "H.264") \(dimensions.width)x\(dimensions.height)",
                                   "display: \(displayPath?.rawValue ?? "none")"])
    }

    public func decode(sample: Data, meta: SampleMeta) -> DecodeResult {
        guard let running = lock.withLock({ current }), let session = running.session else {
            return .failed(error: .Internal)
        }
        let buffer: CMSampleBuffer
        do {
            buffer = try VideoFormat.sampleBuffer(sample, format: running.format, ptsMicros: meta.ptsNs / 1_000,
                                                  keyframe: meta.keyframe)
        } catch {
            return .failed(error: .Internal)
        }
        switch session.decode(buffer, generation: meta.generation, sequence: meta.seq, context: nil) {
        case .accepted:
            return .accepted
        case .busy:
            // An output may have freed a slot between the refusal and here: check again
            // under the lock, so `inputReady` is never missed.
            let free = lock.withLock { () -> Bool in
                if session.inFlight < DecompressionSession.queueBound { return true }
                inputWanted = true
                return false
            }
            if free { running.events.inputReady() }
            return .busy
        case .failed(let status):
            log?.error("StreamCore client: VTDecompressionSessionDecodeFrame failed (\(status))")
            return .failed(error: DecompressionSession.needsNewSession(status) ? .DeviceLost : .Internal)
        }
    }

    public func flush() -> InvalidateOutcome {
        // Forget what waits: outputs of flushed frames are ignored. VideoToolbox keeps no
        // queue of its own beyond them, and the next frame Rust sends is a keyframe.
        lock.withLock { current }?.session?.flush()
        presenter.flush()
        return .completed
    }

    public func setMode(mode: PresentMode) { presenter.setMode(mode) }

    public func nowNs() -> UInt64 { MediaClock.now() }

    // MARK: Outputs (VideoToolbox's thread)

    private func handle(_ output: DecompressionSession.Output, of running: Running) {
        switch output {
        case .decoded(let frame):
            running.events.decoded(seq: frame.sequence, c3Ns: frame.decodedAt)
            if lock.withLock({ current === running }) { presenter.submit(frame) }
        case .dropped:
            // No output for this frame: a later frame's `decoded` retires it.
            break
        case .failed(_, let status):
            log?.error("StreamCore client: decode failed (\(status))")
            // A new session (after an invalidated one, or an error): Rust re-creates and asks for a keyframe.
            running.events.failed(error: DecompressionSession.needsNewSession(status) ? .DeviceLost : .Internal, recoverable: true)
        }
        let wanted = lock.withLock { () -> Bool in
            guard current === running, inputWanted else { return false }
            inputWanted = false
            return true
        }
        if wanted { running.events.inputReady() }
    }

    // MARK: PresentObserver

    public func presentRequested(_ frame: DecompressionSession.Frame, at nanoseconds: UInt64) {
        lock.withLock { current }?.events.presented(seq: frame.sequence, c5Ns: nanoseconds, estimated: true)
    }

    public func presentSuperseded(_ frame: DecompressionSession.Frame) {}

    private static func usesHardware(_ session: VTDecompressionSession) -> Bool {
        var value: CFTypeRef?
        let status = withUnsafeMutablePointer(to: &value) { pointer in
            VTSessionCopyProperty(session, key: kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
                                  allocator: nil, valueOut: UnsafeMutableRawPointer(pointer))
        }
        return status == noErr && (value as? Bool) == true
    }
}
