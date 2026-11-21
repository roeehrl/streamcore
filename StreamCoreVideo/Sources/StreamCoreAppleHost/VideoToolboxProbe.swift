// macOS only: the host side (a Mac captures and encodes).
#if os(macOS)
import CoreMedia
import CoreVideo
import Foundation
import StreamCoreVideo
import VideoToolbox

/// What VideoToolbox on this Mac does for LTR and temporal layers (StreamCore video design
/// 3.3 `EncoderCaps`, 6.5): one small probe session per codec, run once per process at the
/// first stream start. A feature is `confirmed` only when the session took the setting AND
/// its output carried it: LTR tokens on the frames (`RequireLTRAcknowledgementToken`) and an
/// accepted `ForceLTRRefresh` after an acknowledgement that came out as a delta; temporal
/// layers as frames other frames do not depend on (`IsDependedOnByOthers` false) between
/// reference frames. `requested`: the session took the setting but the output did not
/// show it. HEVC is not probed and reports both `unsupported`.
public enum VideoToolboxProbe {
    private static let lock = NSLock()
    private nonisolated(unsafe) static var cache: [Codec: EncoderCaps] = [:]

    /// Cached per codec for the process.
    public static func caps(_ codec: Codec, log: (any MediaLog)? = nil) -> EncoderCaps {
        if let cached = lock.withLock({ cache[codec] }) { return cached }
        let probed = run(codec)
        lock.withLock { cache[codec] = probed }
        log?.info("StreamCore: VideoToolbox \(codec == .h264 ? "H.264" : "HEVC") probe: ltr=\(probed.ltr) temporalLayers=\(probed.temporalLayers) hardware=\(probed.hardware) maxFrameQp=\(probed.maxFrameQp)")
        return probed
    }

    /// One probe, uncached (tests).
    public static func run(_ codec: Codec,
                    sessionFactory: VideoToolboxSession.Factory = VideoToolboxSession.create) -> EncoderCaps {
        var caps = EncoderCaps(codec: codec, name: "VideoToolbox", hardware: false, ltr: .unsupported,
                               temporalLayers: .unsupported, intraRefresh: .unsupported,
                               lowLatencyRateControl: .requested, maxWidth: 8192, maxHeight: 8192, maxFps: 120,
                               inputs: [.nv12, .bgra], forceIdr: true, dynamicRate: true, minInputsBeforeOutput: 1,
                               vuiChecked: true)
        guard codec == .h264 else { return caps }
        let specification = [kVTVideoEncoderSpecification_EnableLowLatencyRateControl: true] as CFDictionary
        let (status, created) = sessionFactory(320, 180, kCMVideoCodecType_H264, specification)
        guard status == noErr, let session = created else { return caps }
        defer { VTCompressionSessionInvalidate(session) }
        caps.lowLatencyRateControl = .confirmed
        func set(_ key: CFString, _ value: CFTypeRef) -> Bool { VTSessionSetProperty(session, key: key, value: value) == noErr }
        _ = set(kVTCompressionPropertyKey_RealTime, kCFBooleanTrue)
        _ = set(kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse)
        _ = set(kVTCompressionPropertyKey_AverageBitRate, NSNumber(value: 1_000_000))
        _ = set(kVTCompressionPropertyKey_ExpectedFrameRate, NSNumber(value: 60))
        let ltrSet = set(kVTCompressionPropertyKey_EnableLTR, kCFBooleanTrue)
        let layersSet = set(kVTCompressionPropertyKey_BaseLayerFrameRateFraction, NSNumber(value: 0.5))
        VTCompressionSessionPrepareToEncodeFrames(session)
        // A low-latency session does not report UsingHardwareAcceleratedVideoEncoder; the
        // encoder it uses tells (VideoToolboxHardware).
        caps.hardware = VideoToolboxHardware.isHardware(session, codec: kCMVideoCodecType_H264)
        // A frame-level maximum QP (design 6.6): the session takes MaxAllowedFrameQP.
        caps.maxFrameQp = set(kVTCompressionPropertyKey_MaxAllowedFrameQP, NSNumber(value: 51))

        var tokens = 0, discardable = 0, reference = 0
        var firstToken: Int64?
        var refreshOK = false
        for index in 0..<10 {
            guard let image = ProbeImage.make(width: 320, height: 180, index: index) else { return caps }
            var options: [CFString: Any] = [:]
            if ltrSet, index == 4, let firstToken { options[kVTEncodeFrameOptionKey_AcknowledgedLTRTokens] = [NSNumber(value: firstToken)] }
            let refreshing = ltrSet && index == 7 && firstToken != nil
            if refreshing { options[kVTEncodeFrameOptionKey_ForceLTRRefresh] = true }
            let done = DispatchSemaphore(value: 0)
            nonisolated(unsafe) var attachments: [CFString: Any]?
            let encodeStatus = VTCompressionSessionEncodeFrame(
                session, imageBuffer: image, presentationTimeStamp: CMTime(value: CMTimeValue(index), timescale: 60),
                duration: .invalid, frameProperties: options.isEmpty ? nil : options as CFDictionary, infoFlagsOut: nil
            ) { status, _, sample in
                defer { done.signal() }
                guard status == noErr, let sample else { return }
                attachments = (CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false)
                    as? [[CFString: Any]])?.first ?? [:]
            }
            guard encodeStatus == noErr else { return caps }
            VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
            guard done.wait(timeout: .now() + 2) == .success, let attachments else { return caps }
            let keyframe = !((attachments[kCMSampleAttachmentKey_NotSync] as? Bool) ?? false)
            if let token = attachments[kVTSampleAttachmentKey_RequireLTRAcknowledgementToken] as? NSNumber {
                tokens += 1
                if firstToken == nil { firstToken = token.int64Value }
            }
            if !keyframe {
                if (attachments[kCMSampleAttachmentKey_IsDependedOnByOthers] as? Bool) == false { discardable += 1 } else { reference += 1 }
            }
            if refreshing {
                refreshOK = !keyframe && ((attachments["ReferenceWasRefreshed" as CFString] as? Bool) ?? true)
            }
        }
        if ltrSet { caps.ltr = tokens >= 2 && refreshOK ? .confirmed : .requested }
        if layersSet { caps.temporalLayers = discardable >= 2 && reference >= 2 ? .confirmed : .requested }
        return caps
    }
}

/// Moving test pictures for probes.
enum ProbeImage {
    static func make(width: Int, height: Int, index: Int) -> CVPixelBuffer? {
        var buffer: CVPixelBuffer?
        let attributes = [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary
        guard CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_32BGRA, attributes, &buffer) == kCVReturnSuccess,
              let buffer else { return nil }
        CVPixelBufferLockBaseAddress(buffer, [])
        defer { CVPixelBufferUnlockBaseAddress(buffer, []) }
        guard let base = CVPixelBufferGetBaseAddress(buffer)?.assumingMemoryBound(to: UInt8.self) else { return nil }
        let stride = CVPixelBufferGetBytesPerRow(buffer)
        for y in 0..<height {
            let row = base + y * stride
            for x in 0..<width {
                let pixel = row + x * 4
                let checker = ((x / 16 + y / 16 + index / 2) & 1) * 180
                pixel[0] = UInt8(checker); pixel[1] = UInt8((x + index * 5) & 255)
                pixel[2] = UInt8((y + index * 3) & 255); pixel[3] = 255
            }
        }
        return buffer
    }
}
#endif
