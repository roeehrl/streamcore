#if os(macOS)
import CoreMedia
import CoreVideo
import Foundation
import StreamCoreVideo
import Testing
import VideoToolbox
@testable import StreamCoreAppleHost

/// Rate adaptation on VideoToolbox (design 6.6): what Apple's low-latency
/// H.264 encoder does with the adapter's rate updates, measured on real sessions, and the
/// encoder's hardware flag.
@Suite(.serialized) struct VideoToolboxRateTests {
    private final class Output: @unchecked Sendable {
        let lock = NSLock()
        var frames: [(index: Int64, bytes: Int, keyframe: Bool)] = []
    }

    /// `count` pre-rendered 1280×720 NV12 pictures: a desktop-like scrolling pattern with a
    /// moving grainy region (video in a window), or (`noise`) a region of random bytes no
    /// encoder compresses.
    private static func pictures(count: Int, noise: Bool) throws -> [CVPixelBuffer] {
        let width = 1280, height = 720
        return try (0..<count).map { step in
            var buffer: CVPixelBuffer?
            try #require(CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &buffer) == kCVReturnSuccess)
            let image = try #require(buffer)
            CVPixelBufferLockBaseAddress(image, [])
            defer { CVPixelBufferUnlockBaseAddress(image, []) }
            let y = CVPixelBufferGetBaseAddressOfPlane(image, 0)!.assumingMemoryBound(to: UInt8.self)
            let stride = CVPixelBufferGetBytesPerRowOfPlane(image, 0)
            var line = [UInt8](repeating: 0, count: width)
            for row in 0..<height {
                for column in 0..<width {
                    line[column] = (row / 6 + column / 9 + step * 3) % 7 == 0 ? 20 : 230
                }
                let target = y + row * stride
                memcpy(target, line, width)
                if row > 200, row < 520 {
                    if noise {
                        arc4random_buf(target + 300, 600)
                    } else {
                        // A moving gradient with camera-like grain.
                        var grain = [UInt8](repeating: 0, count: 600)
                        arc4random_buf(&grain, 600)
                        for column in 300..<900 {
                            target[column] = UInt8(truncatingIfNeeded: (row + column + step * 5) / 3 + Int(grain[column - 300] & 15))
                        }
                    }
                }
            }
            let uv = CVPixelBufferGetBaseAddressOfPlane(image, 1)!
            memset(uv, 0x80, CVPixelBufferGetBytesPerRowOfPlane(image, 1) * CVPixelBufferGetHeightOfPlane(image, 1))
            return image
        }
    }

    struct Phase {
        /// Output bit/s over the phase's frame time.
        var rate: Double
        /// Frames output, and offered.
        var outputs: Int, frames: Int
    }

    /// Encodes 60 fps in real time for each `(seconds, update)` phase, applying the update
    /// through the adapter's `applyRate`; returns each phase after a settling half second.
    private static func encode(_ phases: [(seconds: Int, update: RateUpdate)], noise: Bool) throws -> [Phase] {
        var created: VTCompressionSession?
        let specification = [kVTVideoEncoderSpecification_EnableLowLatencyRateControl: true] as CFDictionary
        try #require(VTCompressionSessionCreate(allocator: nil, width: 1280, height: 720, codecType: kCMVideoCodecType_H264,
            encoderSpecification: specification, imageBufferAttributes: nil, compressedDataAllocator: nil,
            outputCallback: nil, refcon: nil, compressionSessionOut: &created) == noErr)
        let session = try #require(created)
        defer { VTCompressionSessionInvalidate(session) }
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_RealTime, value: kCFBooleanTrue)
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_AllowFrameReordering, value: kCFBooleanFalse)
        let pictures = try pictures(count: 30, noise: noise)
        let output = Output()
        var index: Int64 = 0
        var ranges: [Range<Int64>] = []
        let began = ContinuousClock.now
        for (number, phase) in phases.enumerated() {
            try #require(VideoToolboxStreamEncoder.applyRate(session, phase.update) == noErr)
            if number == 0 { VTCompressionSessionPrepareToEncodeFrames(session) }
            let start = index
            for _ in 0..<(phase.seconds * 60) {
                // In real time, as a capture delivers (a real-time session drops frames
                // offered faster).
                let due = began + .microseconds(index * 16_667)
                if ContinuousClock.now < due { Thread.sleep(forTimeInterval: Double((due - ContinuousClock.now).components.attoseconds) / 1e18) }
                let frame = index
                VTCompressionSessionEncodeFrame(session, imageBuffer: pictures[Int(frame) % pictures.count],
                    presentationTimeStamp: CMTime(value: frame, timescale: 60), duration: CMTime(value: 1, timescale: 60),
                    frameProperties: nil, infoFlagsOut: nil) { status, _, sample in
                    guard status == noErr, let sample else { return }
                    let attachments = (CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false)
                        as? [[CFString: Any]])?.first
                    let keyframe = !((attachments?[kCMSampleAttachmentKey_NotSync] as? Bool) ?? false)
                    output.lock.withLock { output.frames.append((frame, CMSampleBufferGetTotalSampleSize(sample), keyframe)) }
                }
                index += 1
            }
            ranges.append(start + 30..<index)
        }
        VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
        let frames = output.lock.withLock { output.frames }
        print("VideoToolbox rate test: \(frames.count) outputs of \(index) frames, first sizes \(frames.prefix(5).map(\.bytes)), keyframes \(frames.filter(\.keyframe).count)")
        return ranges.map { range in
            let phase = frames.filter { range.contains($0.index) }
            let bytes = phase.reduce(0) { $0 + $1.bytes }
            return Phase(rate: Double(bytes * 8) * 60 / Double(range.count), outputs: phase.count, frames: range.count)
        }
    }

    private static func update(_ bps: UInt64, window: UInt32? = 150, qp: UInt8? = nil) -> RateUpdate {
        RateUpdate(bitrateBps: bps, fps: 60, rateWindowMs: window, maxFrameQp: qp)
    }

    /// Live bit rate changes reach the output within half a second, each within 15 %.
    @Test(.timeLimit(.minutes(2))) func videoToolboxHonoursBitrateChanges() throws {
        let targets: [UInt64] = [8_000_000, 2_000_000, 5_000_000, 12_000_000]
        let rates = try Self.encode(targets.map { (2, Self.update($0)) }, noise: false).map(\.rate)
        print("VideoToolbox low-latency H.264 1280×720, 150 ms window: asked \(targets.map { Double($0) / 1e6 }) Mbit/s, got \(rates.map { ($0 / 1e4).rounded() / 100 })")
        for (target, rate) in zip(targets, rates) {
            #expect(abs(rate / Double(target) - 1) < 0.15, "asked \(target), got \(rate)")
        }
    }

    /// The maximum QP is honoured: on content no encoder compresses, VideoToolbox holds a
    /// 2 Mbit/s target by dropping frames (reported as skipped, not lost); a maximum QP of
    /// 51 (the planner's value) lets it code more of them, coarser, within the target.
    @Test(.timeLimit(.minutes(2))) func aMaximumQpOf51KeepsMoreFramesAtALowTarget() throws {
        guard VideoToolboxProbe.run(.h264).maxFrameQp else {
            print("VideoToolbox H.264: MaxAllowedFrameQP not supported here")
            return
        }
        let ceiling = try Self.encode([(1, Self.update(8_000_000)), (2, Self.update(2_000_000))], noise: true)
        let raised = try Self.encode([(1, Self.update(8_000_000, qp: 51)), (2, Self.update(2_000_000, qp: 51))],
                                     noise: true)
        print("VideoToolbox, incompressible region, 2 Mbit/s asked: \(ceiling[1].rate / 1e6) Mbit/s and \(ceiling[1].outputs)/\(ceiling[1].frames) frames with its own QP ceiling; \(raised[1].rate / 1e6) Mbit/s and \(raised[1].outputs)/\(raised[1].frames) frames with a maximum QP of 51")
        #expect(raised[1].rate < 2_000_000 * 1.15 && ceiling[1].rate < 2_000_000 * 1.15)
        #expect(raised[1].outputs > ceiling[1].outputs + ceiling[1].frames / 10, "more frames coded")
    }

    /// Apple silicon's low-latency session runs on the hardware encoder (`…h264.rtvc`)
    /// but does not report `UsingHardwareAcceleratedVideoEncoder`; the flag comes from the
    /// encoder in use.
    @Test func theHardwareFlagReadsTheEncoderInUse() throws {
        var created: VTCompressionSession?
        let lowLatency = [kVTVideoEncoderSpecification_EnableLowLatencyRateControl: true] as CFDictionary
        try #require(VTCompressionSessionCreate(allocator: nil, width: 640, height: 360, codecType: kCMVideoCodecType_H264,
            encoderSpecification: lowLatency, imageBufferAttributes: nil, compressedDataAllocator: nil,
            outputCallback: nil, refcon: nil, compressionSessionOut: &created) == noErr)
        let session = try #require(created)
        defer { VTCompressionSessionInvalidate(session) }
        var unused: CFTypeRef?
        let reported = withUnsafeMutablePointer(to: &unused) { pointer in
            VTSessionCopyProperty(session, key: kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
                                  allocator: nil, valueOut: UnsafeMutableRawPointer(pointer))
        }
        print("VideoToolbox low-latency H.264: encoder \(VideoToolboxHardware.encoderID(session) ?? "?"), UsingHardwareAcceleratedVideoEncoder status \(reported)")
        #if arch(arm64)
        #expect(VideoToolboxHardware.isHardware(session, codec: kCMVideoCodecType_H264))
        #expect(VideoToolboxProbe.run(.h264).hardware)
        #endif
        // The software encoder (not low latency) reports itself.
        var software: VTCompressionSession?
        let noHardware = [kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: false] as CFDictionary
        try #require(VTCompressionSessionCreate(allocator: nil, width: 640, height: 360, codecType: kCMVideoCodecType_H264,
            encoderSpecification: noHardware, imageBufferAttributes: nil, compressedDataAllocator: nil,
            outputCallback: nil, refcon: nil, compressionSessionOut: &software) == noErr)
        let softwareSession = try #require(software)
        defer { VTCompressionSessionInvalidate(softwareSession) }
        #expect(!VideoToolboxHardware.isHardware(softwareSession, codec: kCMVideoCodecType_H264))
    }
}
#endif
