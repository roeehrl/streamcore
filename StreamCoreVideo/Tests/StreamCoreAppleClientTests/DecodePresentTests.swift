import AVFoundation
import CoreMedia
import CoreVideo
import Foundation
import StreamCoreVideo
import Synchronization
import Testing
import VideoToolbox
@testable import StreamCoreAppleClient

/// Encodes `count` moving pictures with VideoToolbox: the codec configuration atom and
/// each length-prefixed sample.
private func encode(codec: Codec, width: Int = 320, height: Int = 192, count: Int) throws -> (config: Data, samples: [(data: Data, keyframe: Bool)]) {
    final class Output: @unchecked Sendable {
        let lock = NSLock()
        var config: Data?
        var samples: [(Int64, Data, Bool)] = []
    }
    var created: VTCompressionSession?
    let type = codec == .hevc ? kCMVideoCodecType_HEVC : kCMVideoCodecType_H264
    try #require(VTCompressionSessionCreate(allocator: nil, width: Int32(width), height: Int32(height), codecType: type,
        encoderSpecification: nil, imageBufferAttributes: nil, compressedDataAllocator: nil, outputCallback: nil,
        refcon: nil, compressionSessionOut: &created) == noErr)
    let session = try #require(created)
    defer { VTCompressionSessionInvalidate(session) }
    VTSessionSetProperty(session, key: kVTCompressionPropertyKey_RealTime, value: kCFBooleanTrue)
    VTSessionSetProperty(session, key: kVTCompressionPropertyKey_AllowFrameReordering, value: kCFBooleanFalse)
    VTSessionSetProperty(session, key: kVTCompressionPropertyKey_ColorPrimaries, value: kCVImageBufferColorPrimaries_ITU_R_709_2)
    VTSessionSetProperty(session, key: kVTCompressionPropertyKey_TransferFunction, value: kCVImageBufferTransferFunction_ITU_R_709_2)
    VTSessionSetProperty(session, key: kVTCompressionPropertyKey_YCbCrMatrix, value: kCVImageBufferYCbCrMatrix_ITU_R_709_2)
    let output = Output()
    for index in 0..<count {
        var buffer: CVPixelBuffer?
        try #require(CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &buffer) == kCVReturnSuccess)
        let image = try #require(buffer)
        CVPixelBufferLockBaseAddress(image, [])
        let y = CVPixelBufferGetBaseAddressOfPlane(image, 0)!.assumingMemoryBound(to: UInt8.self)
        let stride = CVPixelBufferGetBytesPerRowOfPlane(image, 0)
        for row in 0..<height { for column in 0..<width {
            y[row * stride + column] = UInt8(truncatingIfNeeded: 40 + ((column / 16 + row / 16 + index) & 1) * 150)
        } }
        memset(CVPixelBufferGetBaseAddressOfPlane(image, 1)!, 0x80,
               CVPixelBufferGetBytesPerRowOfPlane(image, 1) * CVPixelBufferGetHeightOfPlane(image, 1))
        CVPixelBufferUnlockBaseAddress(image, [])
        let frame = Int64(index)
        try #require(VTCompressionSessionEncodeFrame(session, imageBuffer: image,
            presentationTimeStamp: CMTime(value: frame, timescale: 60), duration: .invalid, frameProperties: nil,
            infoFlagsOut: nil) { status, _, sample in
            guard status == noErr, let sample, let block = CMSampleBufferGetDataBuffer(sample),
                  let format = CMSampleBufferGetFormatDescription(sample) else { return }
            var data = Data(count: CMBlockBufferGetDataLength(block))
            _ = data.withUnsafeMutableBytes { CMBlockBufferCopyDataBytes(block, atOffset: 0, dataLength: $0.count, destination: $0.baseAddress!) }
            let attachments = (CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false) as? [[CFString: Any]])?.first
            let keyframe = !((attachments?[kCMSampleAttachmentKey_NotSync] as? Bool) ?? false)
            let atoms = CMFormatDescriptionGetExtension(format,
                extensionKey: kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms) as? [String: Any]
            output.lock.withLock {
                output.config = output.config ?? atoms?[codec == .hevc ? "hvcC" : "avcC"] as? Data
                output.samples.append((frame, data, keyframe))
            }
        } == noErr)
    }
    VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
    let config = try #require(output.lock.withLock { output.config })
    return (config, output.lock.withLock { output.samples.sorted { $0.0 < $1.0 }.map { ($0.1, $0.2) } })
}

/// A display that takes samples only while the test lets it.
private final class FakeDisplay: VideoDisplay, @unchecked Sendable {
    private struct State {
        var ready = true, failed = false
        var shown: [CMTimeValue] = []
        var flushes = 0
        var waiters: [(DispatchQueue, @Sendable () -> Void)] = []
    }
    private let state = Mutex(State())
    var path: VideoDisplays.Path { .layer }
    var isReady: Bool { state.withLock { $0.ready } }
    var isFailed: Bool { state.withLock { $0.failed } }
    var shown: [CMTimeValue] { state.withLock { $0.shown } }
    var flushes: Int { state.withLock { $0.flushes } }
    func enqueue(_ sample: sending CMSampleBuffer) {
        let pts = CMSampleBufferGetPresentationTimeStamp(sample).value
        state.withLock { $0.shown.append(pts) }
    }
    func flush() { state.withLock { $0.flushes += 1; $0.failed = false } }
    func whenReady(on queue: DispatchQueue, _ block: @escaping @Sendable () -> Void) {
        state.withLock { $0.waiters.append((queue, block)) }
    }
    func set(ready: Bool) {
        let waiters = state.withLock { state -> [(DispatchQueue, @Sendable () -> Void)] in
            state.ready = ready
            defer { if ready { state.waiters.removeAll() } }
            return ready ? state.waiters : []
        }
        for (queue, block) in waiters { queue.async(execute: block) }
    }
    func fail() { state.withLock { $0.failed = true } }
}

private final class Observer: PresentObserver, @unchecked Sendable {
    let requested = Mutex<[UInt64]>([]), superseded = Mutex<[UInt64]>([])
    func presentRequested(_ frame: DecompressionSession.Frame, at nanoseconds: UInt64) { requested.withLock { $0.append(frame.sequence) } }
    func presentSuperseded(_ frame: DecompressionSession.Frame) { superseded.withLock { $0.append(frame.sequence) } }
}

private func eventually(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now + .seconds(10)
    while !condition(), ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(5)) }
    try #require(condition())
}

/// A decoded image with nothing in it, for presenter tests.
private func frame(_ sequence: UInt64) throws -> DecompressionSession.Frame {
    var buffer: CVPixelBuffer?
    try #require(CVPixelBufferCreate(nil, 32, 32, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &buffer) == kCVReturnSuccess)
    return DecompressionSession.Frame(image: try #require(buffer), presentationTime: CMTime(value: CMTimeValue(sequence), timescale: 60),
                                      generation: 1, sequence: sequence, context: nil, decodedAt: MediaClock.now())
}

@Suite(.serialized) struct DecodePresentTests {
    @Test(.timeLimit(.minutes(2)), arguments: [Codec.h264, .hevc])
    func aStreamDecodesFromItsConfigurationWithExactOutputTimes(codec: Codec) async throws {
        let encoded = try encode(codec: codec, count: 8)
        let format = try VideoFormat.description(codec: codec, config: encoded.config)
        let dimensions = CMVideoFormatDescriptionGetDimensions(format)
        #expect(dimensions.width == 320 && dimensions.height == 192)
        // The colour comes from the parameter sets, not from a guess by size.
        #expect(CMFormatDescriptionGetExtension(format, extensionKey: kCMFormatDescriptionExtension_YCbCrMatrix) as? String
                == kCVImageBufferYCbCrMatrix_ITU_R_709_2 as String)
        #expect(try CMVideoFormatDescriptionGetDimensions(
            VideoFormat.description(codec: codec, config: encoded.config, size: (320, 192))).width == 320)
        #expect(throws: VideoFormat.FormatError.invalidCodecConfig) {
            _ = try VideoFormat.description(codec: codec, config: encoded.config, size: (640, 360))
        }
        #expect(throws: VideoFormat.FormatError.self) { _ = try VideoFormat.description(codec: codec, config: Data([1, 2, 3])) }

        let decoded = Mutex<[(sequence: UInt64, context: String?, width: Int, at: UInt64)]>([])
        let session = try DecompressionSession(format: format) { _, output in
            guard case .decoded(let frame) = output else { return }
            decoded.withLock { $0.append((frame.sequence, frame.context as? String, CVPixelBufferGetWidth(frame.image), frame.decodedAt)) }
        }
        defer { session.invalidate() }
        #expect(encoded.samples.first?.keyframe == true)
        let began = MediaClock.now()
        for (index, sample) in encoded.samples.enumerated() {
            let buffer = try VideoFormat.sampleBuffer(sample.data, format: format, ptsMicros: UInt64(index) * 16_667, keyframe: sample.keyframe)
            // The decoder holds at most two frames: wait for room, as a client stream does.
            try await eventually { session.inFlight < DecompressionSession.queueBound }
            #expect(session.decode(buffer, generation: 1, sequence: UInt64(index + 1), context: "frame \(index + 1)") == .accepted)
        }
        try await eventually { decoded.withLock { $0.count } == encoded.samples.count }
        let frames = decoded.withLock { $0 }
        #expect(frames.map(\.sequence) == Array(1...8))
        #expect(frames.allSatisfy { $0.width == 320 && $0.context == "frame \($0.sequence)" && $0.at >= began })
        #expect(session.inFlight == 0 && !session.isStalled(after: 1_000_000_000))
    }

    @Test func theDecoderRefusesAThirdWaitingFrameAndForgetsFlushedOnes() throws {
        let encoded = try encode(codec: .h264, count: 3)
        let format = try VideoFormat.description(codec: .h264, config: encoded.config)
        let held = Mutex<[@Sendable () -> Void]>([])
        let outputs = Mutex(0)
        // Outputs held back: a decoder that has not finished yet.
        let session = try DecompressionSession(format: format, outputGate: { output in held.withLock { $0.append(output) } }) { _, _ in
            outputs.withLock { $0 += 1 }
        }
        defer { session.invalidate() }
        func sample(_ index: Int) throws -> CMSampleBuffer {
            try VideoFormat.sampleBuffer(encoded.samples[index].data, format: format, ptsMicros: 0, keyframe: encoded.samples[index].keyframe)
        }
        #expect(session.decode(try sample(0), generation: 1, sequence: 1, context: nil) == .accepted)
        #expect(session.decode(try sample(1), generation: 1, sequence: 2, context: nil) == .accepted)
        #expect(session.decode(try sample(2), generation: 1, sequence: 3, context: nil) == .busy)
        #expect(session.inFlight == 2)
        #expect(session.flush().map(\.sequence) == [1, 2])
        VTDecompressionSessionWaitForAsynchronousFrames(session.native)
        for output in held.withLock({ $0 }) { output() }
        #expect(outputs.withLock { $0 } == 0, "outputs of flushed frames are ignored")
        #expect(DecompressionSession.needsNewSession(kVTInvalidSessionErr) && !DecompressionSession.needsNewSession(noErr))
    }

    @Test func theNewestDecodedFrameWinsWhileTheDisplayIsBusy() async throws {
        let display = FakeDisplay(), observer = Observer()
        let presenter = DecodedFramePresenter(mode: .responsive, observer: observer)
        presenter.setDisplay(display)
        #expect(presenter.path == .layer)
        presenter.submit(try frame(1))
        #expect(display.shown == [1])
        display.set(ready: false)
        presenter.submit(try frame(2))
        presenter.submit(try frame(3))
        presenter.submit(try frame(4))
        #expect(display.shown == [1])
        display.set(ready: true)
        try await eventually { display.shown == [1, 4] }
        #expect(observer.requested.withLock { $0 } == [1, 4] && observer.superseded.withLock { $0 } == [2, 3])
        // A failed display is flushed before the next frame.
        display.fail()
        presenter.submit(try frame(5))
        #expect(display.flushes == 1 && display.shown == [1, 4, 5])
    }

    @Test func smoothModePresentsTheNewestFrameAtEachTick() throws {
        let display = FakeDisplay()
        let presenter = DecodedFramePresenter(mode: .smooth)
        presenter.setDisplay(display)
        presenter.submit(try frame(1))
        presenter.submit(try frame(2))
        #expect(display.shown.isEmpty)
        presenter.tick()
        #expect(display.shown == [2])
        presenter.tick()
        #expect(display.shown == [2], "nothing new since the last tick")
        presenter.setMode(.responsive)
        presenter.submit(try frame(3))
        #expect(display.shown == [2, 3])
    }

    /// One decode on two displays: the mirror shows the same frames while it has room, and
    /// never holds the primary back.
    @Test func aMirrorDisplayShowsTheSameFramesWithoutPacingThePrimary() throws {
        let primary = FakeDisplay(), mirror = FakeDisplay()
        let presenter = DecodedFramePresenter(mode: .responsive)
        presenter.setDisplay(primary)
        presenter.addDisplay(mirror)
        presenter.addDisplay(mirror)
        presenter.submit(try frame(1))
        mirror.set(ready: false)
        presenter.submit(try frame(2))
        mirror.set(ready: true)
        presenter.submit(try frame(3))
        #expect(primary.shown == [1, 2, 3] && mirror.shown == [1, 3])
        presenter.removeDisplay(mirror)
        presenter.submit(try frame(4))
        #expect(primary.shown == [1, 2, 3, 4] && mirror.shown == [1, 3])
        presenter.flush()
        #expect(primary.flushes == 1)
    }
}
