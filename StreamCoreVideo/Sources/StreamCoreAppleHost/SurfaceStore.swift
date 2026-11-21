// macOS only: the host side (a Mac captures and encodes).
#if os(macOS)
import CoreVideo
import Foundation
import Metal
import StreamCoreVideo

/// The native slot table shared by the capture backend and the encoder: Rust holds only
/// these ids (3.2). `release` may run on any thread, so every access takes one short
/// lock, never held across a call into Rust.
public final class SurfaceStore: @unchecked Sendable {
    /// When a composite's GPU work completes (design 3.2: consumers wait on the fence).
    public struct Fence: @unchecked Sendable {
        public let event: MTLSharedEvent
        public let value: UInt64
        public init(event: MTLSharedEvent, value: UInt64) { self.event = event; self.value = value }
        public var signalled: Bool { event.signaledValue >= value }
    }
    /// Sendable: immutable once registered (a copy is handed out); the buffer is only read.
    public struct Entry: @unchecked Sendable {
        public let image: CVPixelBuffer
        /// H0 (source present) and H1 (capture callback), native ns; 0 when unknown.
        public let h0: UInt64, h1: UInt64
        public internal(set) var submitted = false
        /// A composite's completion; nil for a capture surface.
        public var fence: Fence? = nil
        /// The quarter turn that shows the image upright (a frame's orientation).
        public var rotation: Rotation = .none
    }
    private let lock = NSLock()
    private var entries: [UInt64: Entry] = [:]
    private var next: UInt64 = 1

    public init() {}

    public func insert(_ image: CVPixelBuffer, h0: UInt64, h1: UInt64, fence: Fence? = nil, rotation: Rotation = .none) -> UInt64 {
        lock.withLock {
            let id = next
            next &+= 1
            entries[id] = Entry(image: image, h0: h0, h1: h1, fence: fence, rotation: rotation)
            return id
        }
    }

    /// A registered image without marking it submitted (the compositor's members).
    public func entry(_ id: UInt64) -> Entry? { lock.withLock { entries[id] } }

    /// The image for `submit`, marking it submitted. `again` is true for a re-encode of
    /// an image already submitted (a recovery, a skipped image, a keep-warm repeat).
    public func submit(_ id: UInt64) -> (entry: Entry, again: Bool)? {
        lock.withLock {
            guard var entry = entries[id] else { return nil }
            let again = entry.submitted
            entry.submitted = true
            entries[id] = entry
            return (entry, again)
        }
    }

    public func remove(_ id: UInt64) -> Entry? { lock.withLock { entries.removeValue(forKey: id) } }

    public var count: Int { lock.withLock { entries.count } }
}

// MARK: - Colour

public enum SurfaceColor {
    /// A capture's colour attachments as StreamCore tags. Values StreamCore cannot name
    /// (SMPTE C, EBU 3213, DCI) are left unspecified, so the encoder keeps its default
    /// for them as VideoToolbox does for an untagged buffer.
    public static func tags(of buffer: CVPixelBuffer) -> ColorTags {
        let primaries = CVBufferCopyAttachment(buffer, kCVImageBufferColorPrimariesKey, nil) as? String
        let transfer = CVBufferCopyAttachment(buffer, kCVImageBufferTransferFunctionKey, nil) as? String
        let matrix = CVBufferCopyAttachment(buffer, kCVImageBufferYCbCrMatrixKey, nil) as? String
        let format = CVPixelBufferGetPixelFormatType(buffer)
        let fullRange = format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
            || format == kCVPixelFormatType_32BGRA
        let primariesByName: [String: ColorPrimaries] = [
            kCVImageBufferColorPrimaries_ITU_R_709_2 as String: .bt709,
            kCVImageBufferColorPrimaries_P3_D65 as String: .displayP3,
            kCVImageBufferColorPrimaries_ITU_R_2020 as String: .bt2020]
        let transferByName: [String: TransferFunction] = [
            kCVImageBufferTransferFunction_ITU_R_709_2 as String: .bt709,
            kCVImageBufferTransferFunction_sRGB as String: .srgb,
            kCVImageBufferTransferFunction_Linear as String: .linear,
            kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ as String: .pq,
            kCVImageBufferTransferFunction_ITU_R_2100_HLG as String: .hlg]
        let matrixByName: [String: YCbCrMatrix] = [
            kCVImageBufferYCbCrMatrix_ITU_R_709_2 as String: .bt709,
            kCVImageBufferYCbCrMatrix_ITU_R_601_4 as String: .bt601,
            kCVImageBufferYCbCrMatrix_ITU_R_2020 as String: .bt2020]
        let p = primaries.flatMap { primariesByName[$0] } ?? .unspecified
        let t = transfer.flatMap { transferByName[$0] } ?? .unspecified
        let m = matrix.flatMap { matrixByName[$0] } ?? .unspecified
        return ColorTags(primaries: p, transfer: t, matrix: m, fullRange: fullRange)
    }

    /// VideoToolbox property values for planned output tags; nil keeps the session's
    /// default (an untagged value).
    public static func properties(_ tags: ColorTags) -> (primaries: CFString?, transfer: CFString?, matrix: CFString?) {
        let primaries: CFString? = switch tags.primaries {
        case .bt709: kCVImageBufferColorPrimaries_ITU_R_709_2
        case .displayP3: kCVImageBufferColorPrimaries_P3_D65
        case .bt2020: kCVImageBufferColorPrimaries_ITU_R_2020
        case .unspecified: nil
        }
        let transfer: CFString? = switch tags.transfer {
        case .bt709: kCVImageBufferTransferFunction_ITU_R_709_2
        case .srgb: kCVImageBufferTransferFunction_sRGB
        case .linear: kCVImageBufferTransferFunction_Linear
        case .pq: kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ
        case .hlg: kCVImageBufferTransferFunction_ITU_R_2100_HLG
        case .unspecified: nil
        }
        let matrix: CFString? = switch tags.matrix {
        case .bt709: kCVImageBufferYCbCrMatrix_ITU_R_709_2
        case .bt601: kCVImageBufferYCbCrMatrix_ITU_R_601_4
        case .bt2020: kCVImageBufferYCbCrMatrix_ITU_R_2020
        case .unspecified, .identity: nil
        }
        return (primaries, transfer, matrix)
    }
}
#endif
