import CoreMedia
import CoreVideo
import Foundation
import StreamCoreVideo
import VideoToolbox

/// One stream's VideoToolbox decoder (design 5, 6.2): a real-time, hardware-preferred
/// `VTDecompressionSession` whose images go to `DecodedFramePresenter`. It holds at most
/// `queueBound` frames between submit and output; the caller flushes and asks for a
/// keyframe beyond that (R4). The output time is the frame's exact C3.
///
/// `context` is the caller's own value for a frame (a trace slot, say), handed back with
/// its output.
///
/// Thread-safe. Outputs arrive on VideoToolbox's thread, or synchronously inside
/// `decode`; `deliver` must not take a lock its caller holds across `decode`.
public final class DecompressionSession: @unchecked Sendable {
    /// Frames submitted and not yet output, at most (design 6.2, "waiting for the decoder").
    public static let queueBound = 2

    public struct Frame: @unchecked Sendable {
        public let image: CVImageBuffer
        public let presentationTime: CMTime
        public let generation: UInt64
        public let sequence: UInt64
        public let context: (any Sendable)?
        /// C3 on the native monotonic clock.
        public let decodedAt: UInt64
    }
    /// A frame known to the decoder: identity for traces and flush events.
    public struct Submitted: Sendable {
        public let generation: UInt64
        public let sequence: UInt64
        public let context: (any Sendable)?
        public let submittedAt: UInt64
    }
    public enum Output: @unchecked Sendable {
        case decoded(Frame)
        /// The decoder dropped the frame without an error.
        case dropped(Submitted)
        case failed(Submitted, OSStatus)
    }
    public enum Submit: Equatable {
        case accepted
        /// `queueBound` frames are already waiting for the decoder.
        case busy
        case failed(OSStatus)
    }

    public let native: VTDecompressionSession
    private let color: [CFString: CFString]
    private let deliver: @Sendable (DecompressionSession, Output) -> Void
    private let outputGate: OutputGate?
    /// Runs each VideoToolbox output, now or later: tests hold outputs back to act as
    /// a decoder that has not finished yet. Nil runs them at once.
    public typealias OutputGate = @Sendable (_ output: @escaping @Sendable () -> Void) -> Void
    private let lock = NSLock()
    private var waiting: [UInt64: Submitted] = [:]
    private var nextToken: UInt64 = 1

    public struct CreateFailed: Error {
        public let status: OSStatus
    }

    /// `fallbackColor`: the tags for whatever the stream's parameter sets leave out
    /// (nil: BT.709).
    public init(format: CMVideoFormatDescription, fallbackColor: ColorTags? = nil, outputGate: OutputGate? = nil,
                deliver: @escaping @Sendable (DecompressionSession, Output) -> Void) throws {
        self.deliver = deliver
        self.outputGate = outputGate
        color = Self.colorTags(format, fallback: fallbackColor)
        var specification: [CFString: Any] = [:]
        #if os(macOS)
        specification[kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder] = true
        #endif
        var session: VTDecompressionSession?
        let status = VTDecompressionSessionCreate(allocator: kCFAllocatorDefault, formatDescription: format,
            decoderSpecification: specification as CFDictionary,
            imageBufferAttributes: Self.outputAttributes(format) as CFDictionary,
            outputCallback: nil, decompressionSessionOut: &session)
        guard status == noErr, let session else { throw CreateFailed(status: status) }
        native = session
        // Real time: decode each frame as it arrives, ahead of background work.
        VTSessionSetProperty(session, key: kVTDecompressionPropertyKey_RealTime, value: kCFBooleanTrue)
    }

    /// Stops the session; outputs still to come are dropped. Owners call this
    /// before letting go, never on VideoToolbox's thread (it may wait for output).
    public func invalidate() {
        flush()
        VTDecompressionSessionInvalidate(native)
    }

    /// Frames waiting for the decoder now.
    public var inFlight: Int { lock.withLock { waiting.count } }

    /// Whether the oldest waiting frame was submitted more than `nanoseconds` ago:
    /// a decoder that stopped producing output.
    public func isStalled(after nanoseconds: UInt64, now: UInt64 = MediaClock.now()) -> Bool {
        lock.withLock { waiting.values.contains { now &- $0.submittedAt > nanoseconds } }
    }

    /// Submits one compressed frame (C2 is the caller's, just before); refuses a
    /// frame beyond `queueBound`. Output usually follows on VideoToolbox's thread,
    /// sometimes before this returns.
    public func decode(_ sample: CMSampleBuffer, generation: UInt64, sequence: UInt64, context: (any Sendable)?) -> Submit {
        let token: UInt64? = lock.withLock {
            guard waiting.count < Self.queueBound else { return nil }
            let token = nextToken
            nextToken += 1
            waiting[token] = Submitted(generation: generation, sequence: sequence, context: context,
                                       submittedAt: MediaClock.now())
            return token
        }
        guard let token else { return .busy }
        let status = VTDecompressionSessionDecodeFrame(native, sampleBuffer: sample,
            flags: [._EnableAsynchronousDecompression, ._1xRealTimePlayback], infoFlagsOut: nil) {
            [weak self] status, info, image, presentationTime, _ in
            guard let self else { return }
            let image = image.map(SendableImage.init)
            let output: @Sendable () -> Void = { [weak self] in
                self?.output(token, status: status, info: info, image: image?.buffer, presentationTime: presentationTime)
            }
            if let outputGate = self.outputGate { outputGate(output) } else { output() }
        }
        guard status == noErr else {
            lock.withLock { waiting[token] = nil }
            return .failed(status)
        }
        return .accepted
    }

    /// Forgets every waiting frame (R4 flush): their outputs, if any, are ignored.
    /// Returns them, oldest first, for the trace.
    @discardableResult public func flush() -> [Submitted] {
        lock.withLock {
            let dropped = waiting.sorted { $0.key < $1.key }.map(\.value)
            waiting.removeAll()
            return dropped
        }
    }

    /// Statuses after which this session cannot decode again: VideoToolbox
    /// invalidated it (an iOS app returning from the background), the hardware
    /// decoder failed, or it is unavailable for now.
    public static func needsNewSession(_ status: OSStatus) -> Bool {
        [kVTInvalidSessionErr, kVTVideoDecoderMalfunctionErr, kVTVideoDecoderNotAvailableNowErr].contains(status)
    }

    private func output(_ token: UInt64, status: OSStatus, info: VTDecodeInfoFlags, image: CVImageBuffer?,
                        presentationTime: CMTime) {
        let decodedAt = MediaClock.now()
        guard let submitted = lock.withLock({ waiting.removeValue(forKey: token) }) else { return }  // flushed
        guard status == noErr else { deliver(self, .failed(submitted, status)); return }
        guard let image, !info.contains(.frameDropped) else { deliver(self, .dropped(submitted)); return }
        for (key, value) in color where CVBufferCopyAttachment(image, key, nil) == nil {
            CVBufferSetAttachment(image, key, value, .shouldPropagate)
        }
        deliver(self, .decoded(Frame(image: image, presentationTime: presentationTime,
                                     generation: submitted.generation, sequence: submitted.sequence,
                                     context: submitted.context, decodedAt: decodedAt)))
    }

    /// The output format the layer shows without a conversion: the decoder's own
    /// bi-planar 4:2:0, at the stream's range and bit depth, IOSurface-backed.
    public static func outputAttributes(_ format: CMVideoFormatDescription) -> [CFString: Any] {
        let fullRange = CMFormatDescriptionGetExtension(format,
            extensionKey: kCMFormatDescriptionExtension_FullRangeVideo) as? Bool ?? false
        let bits = CMFormatDescriptionGetExtension(format,
            extensionKey: kCMFormatDescriptionExtension_BitsPerComponent) as? Int ?? 8
        let pixelFormat = switch (bits > 8, fullRange) {
        case (false, false): kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        case (false, true): kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
        case (true, false): kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
        case (true, true): kCVPixelFormatType_420YpCbCr10BiPlanarFullRange
        }
        return [kCVPixelBufferPixelFormatTypeKey: pixelFormat,
                kCVPixelBufferIOSurfacePropertiesKey: [:] as [String: Any],
                kCVPixelBufferMetalCompatibilityKey: true]
    }

    /// The image tags the layer converts with: the stream's own (parsed from its
    /// parameter sets), else `fallback`'s, else BT.709, which every StreamCore host
    /// encodes (design 3.3). Untagged, the layer would guess by size and pick BT.601 for
    /// small frames.
    public static func colorTags(_ format: CMVideoFormatDescription, fallback: ColorTags? = nil) -> [CFString: CFString] {
        let primaries: CFString = switch fallback?.primaries {
        case .displayP3: kCVImageBufferColorPrimaries_P3_D65
        case .bt2020: kCVImageBufferColorPrimaries_ITU_R_2020
        default: kCVImageBufferColorPrimaries_ITU_R_709_2
        }
        let transfer: CFString = switch fallback?.transfer {
        case .srgb: kCVImageBufferTransferFunction_sRGB
        case .linear: kCVImageBufferTransferFunction_Linear
        case .pq: kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ
        case .hlg: kCVImageBufferTransferFunction_ITU_R_2100_HLG
        default: kCVImageBufferTransferFunction_ITU_R_709_2
        }
        let matrix: CFString = switch fallback?.matrix {
        case .bt601: kCVImageBufferYCbCrMatrix_ITU_R_601_4
        case .bt2020: kCVImageBufferYCbCrMatrix_ITU_R_2020
        default: kCVImageBufferYCbCrMatrix_ITU_R_709_2
        }
        let pairs: [(CFString, CFString, CFString)] = [
            (kCVImageBufferColorPrimariesKey, kCMFormatDescriptionExtension_ColorPrimaries, primaries),
            (kCVImageBufferTransferFunctionKey, kCMFormatDescriptionExtension_TransferFunction, transfer),
            (kCVImageBufferYCbCrMatrixKey, kCMFormatDescriptionExtension_YCbCrMatrix, matrix),
        ]
        var tags: [CFString: CFString] = [:]
        for (key, ext, fallback) in pairs {
            tags[key] = (CMFormatDescriptionGetExtension(format, extensionKey: ext) as? String as CFString?) ?? fallback
        }
        return tags
    }
}

/// A decoded image crossing to a deferred output (immutable once output).
private struct SendableImage: @unchecked Sendable { let buffer: CVImageBuffer }
