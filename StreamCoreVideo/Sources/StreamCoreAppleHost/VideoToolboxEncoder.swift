// macOS only: the host side (a Mac captures and encodes).
#if os(macOS)
import CoreMedia
import Foundation
import Metal
import StreamCoreVideo
import VideoToolbox

/// How `VTCompressionSession`s are made, sized and read: the session factory tests
/// replace, the bit rate for a stream's size and quality, and the codec configuration
/// and sync flag of an encoded sample.
public enum VideoToolboxSession {
    public typealias Factory = (Int32, Int32, CMVideoCodecType, CFDictionary) -> (OSStatus, VTCompressionSession?)

    public static func create(width: Int32, height: Int32, codec: CMVideoCodecType,
                       specification: CFDictionary) -> (OSStatus, VTCompressionSession?) {
        var session: VTCompressionSession?
        let status = VTCompressionSessionCreate(allocator: nil, width: width, height: height,
            codecType: codec, encoderSpecification: specification, imageBufferAttributes: nil,
            compressedDataAllocator: nil, outputCallback: nil, refcon: nil,
            compressionSessionOut: &session)
        return (status, session)
    }

    /// A default bit rate for a stream: bits per pixel per frame scale with `quality`
    /// (0...1), clamped to 2...80 Mbit/s.
    public static func bitrate(width: Int, height: Int, fps: Int, quality: Double) -> Int {
        let bpp = 0.02 + max(0, min(1, quality)) * 0.18
        let rate = Double(width * height) * Double(fps) * bpp
        return Int(max(2_000_000, min(80_000_000, rate)))
    }

    static func isSync(_ sampleBuffer: CMSampleBuffer) -> Bool {
        guard let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: false),
              CFArrayGetCount(attachments) > 0 else { return true }
        let dict = unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFDictionary.self)
        let key = Unmanaged.passUnretained(kCMSampleAttachmentKey_NotSync).toOpaque()
        return !CFDictionaryContainsKey(dict, key)
    }

    /// The raw `avcC`/`hvcC` atom of a sample's format.
    static func configAtom(from format: CMFormatDescription, codec: Codec) -> Data? {
        guard let atoms = CMFormatDescriptionGetExtension(
                format, extensionKey: kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms)
                as? [String: Any] else { return nil }
        return atoms[codec == .hevc ? "hvcC" : "avcC"] as? Data
    }
}

/// StreamCore's `EncoderFactory` over VideoToolbox (design 4.1).
/// `create` runs on the executor's bounded helper thread. An HEVC session that cannot
/// be created reports `Unsupported`, and the executor retries with H.264 and announces
/// that codec. A colour change is a new session created by the executor on its next
/// wake (2.5.5), never inside `submit`.
public final class VideoToolboxEncoderFactory: EncoderFactory, @unchecked Sendable {
    public typealias SessionFactory = VideoToolboxSession.Factory
    private let surfaces: SurfaceStore
    private let trace: (any FrameTrace)?
    private let meter: (any FrameMeter)?
    private let log: (any MediaLog)?
    private let sessionFactory: SessionFactory
    private let authorize: @Sendable () -> Bool
    private let lock = NSLock()
    private var made: [VideoToolboxStreamEncoder] = []

    /// `authorize` runs on the output thread before each output reaches Rust: false drops
    /// the output (the product's authority over the source lapsed; the caller revokes the
    /// stream's sink).
    public init(surfaces: SurfaceStore, trace: (any FrameTrace)? = nil, meter: (any FrameMeter)? = nil,
                log: (any MediaLog)? = nil, authorize: @escaping @Sendable () -> Bool = { true },
                sessionFactory: @escaping SessionFactory = VideoToolboxSession.create) {
        self.surfaces = surfaces; self.trace = trace; self.meter = meter; self.log = log
        self.authorize = authorize
        self.sessionFactory = sessionFactory
    }

    /// Every session this factory created, oldest first (tests).
    public var sessions: [VideoToolboxStreamEncoder] { lock.withLock { made } }

    /// What VideoToolbox on this Mac does, from a probe session per codec (cached for the
    /// process): LTR and temporal layers are `confirmed` only when the session took the
    /// settings and its output carried them (`VideoToolboxProbe`).
    public func probe(adapter: UInt64?) -> [EncoderCaps] {
        [VideoToolboxProbe.caps(.h264, log: log), VideoToolboxProbe.caps(.hevc, log: log)]
    }

    public func create(spec: EncoderSpec, events: EncoderEvents) throws -> StreamEncoder {
        let encoder = try VideoToolboxStreamEncoder(spec: spec, events: events, surfaces: surfaces,
                                                    trace: trace, meter: meter, log: log, authorize: authorize,
                                                    sessionFactory: sessionFactory)
        // Per-frame submits go through StreamCore's C ABI; the stream holds `encoder`.
        StreamCoreNative.attachEncoder(sink: spec.sink.nativeKey(), encoder: encoder,
                                       submit: VideoToolboxStreamEncoder.nativeSubmit)
        lock.withLock { made.append(encoder) }
        return encoder
    }
}

/// One `VTCompressionSession`. `submit`, `setRate` and `invalidate` run on the executor;
/// the output handler runs on VideoToolbox's thread and only hands the output to Rust or
/// posts `dropped` (reentrancy rule 4). Submits and outputs go through StreamCore's
/// per-frame C ABI (`nativeSubmit`, `StreamCoreNative.encoded`, bytes copied once,
/// straight from the sample's block buffer); the UniFFI `submit` stays for completeness.
/// VideoToolbox reports a session error only per frame (the encode call's status or the
/// frame's output handler), never asynchronously with no frame, so this adapter has no
/// use for `EncoderEvents.sessionFailed`: each error travels as its frame's drop reason.
public final class VideoToolboxStreamEncoder: StreamEncoder, @unchecked Sendable {
    public let codec: Codec
    /// Held so the sink (and the permits it keeps for frames submitted through the C ABI)
    /// lives while VideoToolbox may still output, as the UniFFI path's permits do.
    private let sink: VideoSinkHandle
    private let events: EncoderEvents
    private let surfaces: SurfaceStore
    private let trace: (any FrameTrace)?
    private let meter: (any FrameMeter)?
    private let log: (any MediaLog)?
    /// The product's authority over the source, checked on the output thread before each output reaches Rust.
    private let authorize: @Sendable () -> Bool
    private let hardware: Bool
    /// LTR is on in this session (`spec.ltr`, H.264, VideoToolbox took `EnableLTR`).
    public let ltr: Bool
    /// Two temporal layers are on (`spec.temporalLayers`, H.264, VideoToolbox took the
    /// base-layer fraction): layer-1 frames are non-reference and reported discardable.
    public let temporalLayers: Bool
    /// The newest LTR token (wire numbering) passed to VideoToolbox as acknowledged: what
    /// a refresh is predicted from. Executor thread only.
    private var acknowledged: UInt32?
    private let lock = NSLock()
    private var session: VTCompressionSession?
    /// This encoder's sink in StreamCore's C ABI (`spec.sink.nativeKey()`).
    private let sinkKey: UInt64
    /// Output hook for tests: called with each output before it is handed to Rust;
    /// returning false drops it as a no-output completion.
    var outputFilter: (@Sendable (UInt64) -> Bool)?
    /// How `invalidate` completes the session's pending frames (tests slow it down).
    var completeFrames: @Sendable (VTCompressionSession) -> Void = { session in
        VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
    }
    /// `invalidate`'s own bound, below StreamCore's 500 ms (3.5).
    static let invalidateBound: DispatchTimeInterval = .milliseconds(450)
    /// Composites whose GPU work was not complete at submit: VideoToolbox cannot wait on
    /// a GPU fence, so each such frame is encoded from the fence's notification, in
    /// submit order, on this listener's queue.
    private let fenceListener = MTLSharedEventListener(dispatchQueue: DispatchQueue(label: "streamcore.video.fence",
                                                                                  qos: .userInteractive))
    private let deferred = DispatchGroup()
    private var deferredCount = 0

    init(spec: EncoderSpec, events: EncoderEvents, surfaces: SurfaceStore,
         trace: (any FrameTrace)?, meter: (any FrameMeter)?, log: (any MediaLog)? = nil,
         authorize: @escaping @Sendable () -> Bool = { true },
         sessionFactory: VideoToolboxSession.Factory) throws {
        guard (2...8192).contains(spec.width), (2...8192).contains(spec.height), (1...120).contains(spec.fps) else {
            throw MediaError.InvalidSettings
        }
        sink = spec.sink; self.events = events; self.surfaces = surfaces
        self.trace = trace; self.meter = meter; self.log = log; self.authorize = authorize
        sinkKey = spec.sink.nativeKey()
        let codecType: CMVideoCodecType = spec.codec == .hevc ? kCMVideoCodecType_HEVC : kCMVideoCodecType_H264
        guard spec.codec != .av1 else { throw MediaError.Unsupported }
        let specification = [kVTVideoEncoderSpecification_EnableLowLatencyRateControl: true] as CFDictionary
        let (status, created) = sessionFactory(Int32(spec.width), Int32(spec.height), codecType, specification)
        guard status == noErr, let created else {
            if let created { VTCompressionSessionInvalidate(created) }
            log?.error("StreamCore: \(spec.codec == .hevc ? "HEVC" : "H.264") session create failed (\(status))")
            // HEVC: the executor falls back to H.264 for a first session.
            throw spec.codec == .hevc ? MediaError.Unsupported : MediaError.EncoderFailed(recoverable: false)
        }
        codec = spec.codec
        session = created
        func set(_ key: CFString, _ value: CFTypeRef) { VTSessionSetProperty(created, key: key, value: value) }
        // Real time, no reordering, a keyframe at least every 600 frames or 10 s (the
        // client asks for one whenever it needs to resync).
        set(kVTCompressionPropertyKey_RealTime, kCFBooleanTrue)
        set(kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse)
        set(kVTCompressionPropertyKey_MaxKeyFrameInterval, NSNumber(value: 600))
        set(kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, NSNumber(value: 10))
        let color = SurfaceColor.properties(spec.color)
        if let value = color.primaries { set(kVTCompressionPropertyKey_ColorPrimaries, value) }
        if let value = color.transfer { set(kVTCompressionPropertyKey_TransferFunction, value) }
        if let value = color.matrix { set(kVTCompressionPropertyKey_YCbCrMatrix, value) }
        hardware = VideoToolboxHardware.isHardware(created, codec: codecType)
        // LTR and two temporal layers (design 6.5) only on H.264, where the probe confirms
        // them; the executor asks only when the plan has them.
        let layered = spec.codec == .h264 && spec.temporalLayers
            && VTSessionSetProperty(created, key: kVTCompressionPropertyKey_BaseLayerFrameRateFraction,
                                    value: NSNumber(value: 0.5)) == noErr
        let ltr = spec.codec == .h264 && spec.ltr
            && VTSessionSetProperty(created, key: kVTCompressionPropertyKey_EnableLTR, value: kCFBooleanTrue) == noErr
        temporalLayers = layered; self.ltr = ltr
        if spec.ltr != ltr || spec.temporalLayers != layered {
            log?.error("StreamCore: VideoToolbox refused LTR \(spec.ltr && !ltr) or temporal layers \(spec.temporalLayers && !layered)")
        }
        _ = Self.applyRate(created, RateUpdate(bitrateBps: spec.bitrateBps, fps: spec.fps))
        VTCompressionSessionPrepareToEncodeFrames(created)
    }

    deinit { if let session { VTCompressionSessionInvalidate(session) } }

    /// Applies a rate (StreamCore design 6.6) to `session`; returns the average bit rate's
    /// status. `fps` is only a hint to VideoToolbox (`ExpectedFrameRate`): StreamCore's
    /// pacer admits the frame rate. With `rateWindowMs`, `DataRateLimits` holds the bit
    /// rate over that window; without, bursts of 1.5× the average over one second. `maxFrameQp` sets `MaxAllowedFrameQP` (StreamCore sends it only when
    /// the probe confirmed it). Measured on Apple silicon's low-latency encoder: bit rate
    /// changes take effect within half a second; the 150 ms window changes nothing
    /// measurable (the low-latency rate control already holds 150 ms windows within about
    /// 1.15× except around an IDR, whose size neither setting limits); and its default QP
    /// ceiling keeps incompressible content far above a low target (2 Mbit/s asked, 15.5
    /// delivered), which a maximum QP of 51 brings to 3.9.
    @discardableResult
    public static func applyRate(_ session: VTCompressionSession, _ update: RateUpdate) -> OSStatus {
        let rate = Int(min(update.bitrateBps, UInt64(Int.max)))
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_ExpectedFrameRate, value: NSNumber(value: update.fps))
        let status = VTSessionSetProperty(session, key: kVTCompressionPropertyKey_AverageBitRate, value: NSNumber(value: rate))
        let limits: [NSNumber] = if let window = update.rateWindowMs, window > 0 {
            [NSNumber(value: rate / 8 * Int(window) / 1000), NSNumber(value: Double(window) / 1000)]
        } else {
            [NSNumber(value: rate * 3 / 16), NSNumber(value: 1.0)]
        }
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_DataRateLimits, value: limits as CFArray)
        if let qp = update.maxFrameQp {
            VTSessionSetProperty(session, key: kVTCompressionPropertyKey_MaxAllowedFrameQP, value: NSNumber(value: qp))
        }
        return status
    }

    public func info() -> EncoderInfo {
        EncoderInfo(codec: codec, name: "VideoToolbox", hardware: hardware, lowLatency: true)
    }

    public func setRate(update: RateUpdate) throws {
        guard let session = lock.withLock({ self.session }) else { return }
        if Self.applyRate(session, update) != noErr { throw MediaError.Unsupported }
    }

    public func submit(frame: UInt64, opts: SubmitOpts, permit: VideoWorkPermit) -> SubmitResult {
        meter?.native(sequence: opts.submitSeq, sent: false) { encode(frame: frame, opts: opts, permit: permit) } ??
            encode(frame: frame, opts: opts, permit: permit)
    }

    /// `submit` through the C ABI (the context is this encoder, which the stream holds);
    /// the stream keeps the frame's permit until its output or drop is posted.
    public static let nativeSubmit: StreamCoreNative.SubmitFunction = { encoder, frame, sequence, ptsNs, captureNs, keyframe,
                                                                  acknowledged, refresh, keepWarmRepeat in
        let encoder = Unmanaged<VideoToolboxStreamEncoder>.fromOpaque(encoder!).takeUnretainedValue()
        let opts = SubmitOpts(submitSeq: sequence, ptsNs: ptsNs, captureNs: captureNs, forceKeyframe: keyframe,
                              ltrRefresh: refresh, ltrAcknowledged: acknowledged == 0 ? nil : acknowledged,
                              keepWarmRepeat: keepWarmRepeat)
        let encode = { encoder.encode(frame: frame, opts: opts, permit: nil) }
        let result = if let meter = encoder.meter { meter.native(sequence: sequence, sent: false, encode) } else { encode() }
        return switch result {
        case .submitted: StreamCoreNative.submitted
        case .busy: StreamCoreNative.busy
        case .failed(let error): StreamCoreNative.failed(error)
        }
    }

    /// `permit` is nil through the C ABI, where the stream keeps it.
    private func encode(frame: UInt64, opts: SubmitOpts, permit: VideoWorkPermit?) -> SubmitResult {
        guard let session = lock.withLock({ self.session }) else { return .failed(error: .Internal) }
        guard let (entry, again) = surfaces.submit(frame) else { return .failed(error: .Internal) }
        if let fence = entry.fence, !fence.signalled || lock.withLock({ deferredCount > 0 }) {
            encodeWhenComplete(fence, entry: entry, again: again, opts: opts, permit: permit)
            return .submitted
        }
        return encodeNow(session, entry: entry, again: again, opts: opts, permit: permit)
    }

    /// Submitted at once (it never blocks the executor); encoded once the composite's GPU
    /// work signalled its fence. A frame that cannot be encoded then is dropped.
    private func encodeWhenComplete(_ fence: SurfaceStore.Fence, entry: SurfaceStore.Entry, again: Bool,
                                    opts: SubmitOpts, permit: VideoWorkPermit?) {
        deferred.enter()
        lock.withLock { deferredCount += 1 }
        let events = events
        fence.event.notify(fenceListener, atValue: fence.value) { [weak self] _, _ in
            defer {
                self?.lock.withLock { self?.deferredCount -= 1 }
                self?.deferred.leave()
            }
            guard let self, let session = self.lock.withLock({ self.session }) else {
                events.dropped(submitSeq: opts.submitSeq, reason: .noOutput)
                return
            }
            switch self.encodeNow(session, entry: entry, again: again, opts: opts, permit: permit) {
            case .submitted: break
            case .busy: events.dropped(submitSeq: opts.submitSeq, reason: .noOutput)
            case .failed(let error): events.dropped(submitSeq: opts.submitSeq, reason: .error(error: error))
            }
        }
    }

    private func encodeNow(_ session: VTCompressionSession, entry: SurfaceStore.Entry, again: Bool,
                           opts: SubmitOpts, permit: VideoWorkPermit?) -> SubmitResult {
        var options: [CFString: Any] = [:]
        if opts.forceKeyframe { options[kVTEncodeFrameOptionKey_ForceKeyFrame] = true }
        var refreshFrom: UInt32?
        if ltr {
            if let token = opts.ltrAcknowledged, let vtToken = Self.vtToken(token) {
                // VideoToolbox predicts a refresh only from tokens it was told were decoded.
                options[kVTEncodeFrameOptionKey_AcknowledgedLTRTokens] = [NSNumber(value: vtToken)] as CFArray
                acknowledged = token
            }
            if opts.ltrRefresh, !opts.forceKeyframe, let acknowledged {
                options[kVTEncodeFrameOptionKey_ForceLTRRefresh] = true
                refreshFrom = acknowledged
            }
        }
        let properties: CFDictionary? = options.isEmpty ? nil : options as CFDictionary
        let refreshToken = refreshFrom
        let sequence = opts.submitSeq, ptsNs = opts.ptsNs
        // H3 is taken as the frame goes in: the output (H4) may arrive before the call returns.
        let input = MediaClock.now()
        trace?.submitted(sequence: sequence, h0: entry.h0, h1: entry.h1, h3: input,
                         resubmit: Resubmit(again: again, opts: opts))
        let status = VTCompressionSessionEncodeFrame(session, imageBuffer: entry.image,
            presentationTimeStamp: CMTime(value: CMTimeValue(ptsNs), timescale: 1_000_000_000),
            duration: .invalid, frameProperties: properties, infoFlagsOut: nil) { [weak self, events] status, flags, sample in
            // Keep the permit until this frame's output or drop has been posted.
            withExtendedLifetime(permit) {
                guard let self else { events.dropped(submitSeq: sequence, reason: .noOutput); return }
                self.output(status: status, sample: sample, sequence: sequence, ptsNs: ptsNs, input: input,
                            refreshFrom: refreshToken, skipped: sample == nil && flags.contains(.frameDropped))
            }
        }
        switch status {
        case noErr: return .submitted
        case kVTVideoEncoderNotAvailableNowErr:
            // Busy: the retained frame is submitted again once the encoder can take it.
            DispatchQueue.global(qos: .userInteractive).asyncAfter(deadline: .now() + .milliseconds(5)) { [events] in
                events.inputReady()
            }
            return .busy
        default:
            log?.error("StreamCore: VTCompressionSessionEncodeFrame failed (\(status))")
            return .failed(error: .EncoderFailed(recoverable: false))
        }
    }

    /// VideoToolbox numbers LTR frames from 0; the wire's tokens are nonzero (9.4), so wire
    /// token = VideoToolbox token + 1.
    static func wireToken(_ vtToken: Int64) -> UInt32? {
        guard vtToken >= 0, vtToken < Int64(UInt32.max) else { return nil }
        return UInt32(vtToken) + 1
    }
    static func vtToken(_ wireToken: UInt32) -> Int64? { wireToken == 0 ? nil : Int64(wireToken) - 1 }

    /// The layer and LTR state of one output (design 3.4 `EncodedFrame`, 9.4 TLVs).
    struct LayerInfo: Equatable {
        var discardable = false
        var temporalID: UInt8?
        var ltr: LtrInfo?
    }

    /// From the sample's attachments: a layer-1 frame is one no other frame depends on
    /// (`IsDependedOnByOthers` false); an LTR candidate carries its token
    /// (`RequireLTRAcknowledgementToken`). A frame submitted as a refresh is reported as one
    /// (predicted from `refreshFrom`) unless it came out as a keyframe, or VideoToolbox's
    /// own `ReferenceWasRefreshed` attachment (not in the SDK headers; read when present)
    /// says it was not, or, with temporal layers, it came out as a layer-1 frame: then no
    /// later frame predicts from it, and the next layer-0 frame still predicts from the
    /// lost one (VideoToolbox reports such a frame as refreshed all the same). StreamCore
    /// then escalates to an IDR.
    static func layerInfo(_ attachments: [CFString: Any], keyframe: Bool, ltr: Bool, temporalLayers: Bool,
                          refreshFrom: UInt32?) -> LayerInfo {
        var info = LayerInfo()
        if temporalLayers {
            let dependedOn = (attachments[kCMSampleAttachmentKey_IsDependedOnByOthers] as? Bool) ?? true
            info.discardable = !keyframe && !dependedOn
            info.temporalID = info.discardable ? 1 : 0
        }
        guard ltr else { return info }
        let refreshed = (attachments["ReferenceWasRefreshed" as CFString] as? Bool) ?? true
        if let refreshFrom, !keyframe, refreshed, !info.discardable {
            info.ltr = LtrInfo(token: refreshFrom, flags: 0x02)
        } else if let number = attachments[kVTSampleAttachmentKey_RequireLTRAcknowledgementToken] as? NSNumber,
                  let token = wireToken(number.int64Value) {
            info.ltr = LtrInfo(token: token, flags: 0x01)
        }
        return info
    }

    private func output(status: OSStatus, sample: CMSampleBuffer?, sequence: UInt64, ptsNs: UInt64, input: UInt64,
                        refreshFrom: UInt32?, skipped: Bool = false) {
        let arrived = MediaClock.now()
        let dropped: (DropReason) -> Void = { [events, meter, trace] reason in
            if case .noOutput = reason { trace?.encoderDrop(sequence: sequence) }
            if let meter { meter.output(sequence: sequence) { events.dropped(submitSeq: sequence, reason: reason) } }
            else { events.dropped(submitSeq: sequence, reason: reason) }
        }
        guard status == noErr else { dropped(.error(error: .EncoderFailed(recoverable: false))); return }
        // The low-latency rate control drops frames to hold a low bit rate on hard content
        // (measured: more than half at 2 Mbit/s with an incompressible region): never
        // encoded, so not a loss for StreamCore (design 6.6).
        if skipped, outputFilter?(sequence) ?? true { dropped(.skipped); return }
        guard let sample, CMSampleBufferDataIsReady(sample), let block = CMSampleBufferGetDataBuffer(sample),
              let format = CMSampleBufferGetFormatDescription(sample),
              let atom = VideoToolboxSession.configAtom(from: format, codec: codec),
              outputFilter?(sequence) ?? true, authorize() else { dropped(.noOutput); return }
        let keyframe = VideoToolboxSession.isSync(sample)
        trace?.encoded(sequence: sequence, keyframe: keyframe, h4: arrived)
        var length = 0, contiguous = 0
        var pointer: UnsafeMutablePointer<CChar>?
        guard CMBlockBufferGetDataPointer(block, atOffset: 0, lengthAtOffsetOut: &contiguous, totalLengthOut: &length,
                                          dataPointerOut: &pointer) == noErr,
              length > 0, length <= 32 * 1024 * 1024 - 29, atom.count <= 32 * 1024 * 1024 - 29 else { dropped(.noOutput); return }
        // VideoToolbox emits 4-byte length-prefixed samples and the avcC/hvcC atom; the
        // configuration goes with every keyframe (2.5.4). StreamCore copies the bytes once,
        // straight from the block buffer when it is contiguous. Not taken: StreamCore already
        // counts the frame as dropped (or the encoder was replaced); nothing more is posted.
        var encoded = NativeEncodedOutput(submitSeq: sequence, ptsNs: ptsNs, keyframe: keyframe,
                                          inputNs: input, outputNs: arrived)
        if ltr || temporalLayers {
            let attachments = (CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false)
                as? [[CFString: Any]])?.first ?? [:]
            let layer = Self.layerInfo(attachments, keyframe: keyframe, ltr: ltr, temporalLayers: temporalLayers,
                                       refreshFrom: refreshFrom)
            encoded.discardable = layer.discardable; encoded.temporalId = layer.temporalID; encoded.ltr = layer.ltr
        }
        let output = encoded
        let sinkKey = self.sinkKey
        let hand: () -> Void = { [events, trace, sink] in
            withExtendedLifetime(sink) {}
            func send(_ payload: UnsafeRawBufferPointer) {
                if keyframe {
                    _ = atom.withUnsafeBytes { StreamCoreNative.encoded(sink: sinkKey, output, payload: payload, config: $0) }
                } else {
                    _ = StreamCoreNative.encoded(sink: sinkKey, output, payload: payload)
                }
            }
            if let pointer, contiguous == length {
                send(UnsafeRawBufferPointer(start: pointer, count: length))
                return
            }
            var copied = Data(count: length)
            let status = copied.withUnsafeMutableBytes { raw -> OSStatus in
                guard let base = raw.baseAddress else { return -1 }
                return CMBlockBufferCopyDataBytes(block, atOffset: 0, dataLength: length, destination: base)
            }
            if status == noErr { copied.withUnsafeBytes(send) } else {
                trace?.encoderDrop(sequence: sequence)
                events.dropped(submitSeq: sequence, reason: .noOutput)
            }
        }
        if let meter { meter.output(sequence: sequence, hand) } else { hand() }
    }

    /// A session handed to the helper that ends it; nothing else uses it then.
    private final class Ending: @unchecked Sendable {
        let session: VTCompressionSession
        init(_ session: VTCompressionSession) { self.session = session }
    }

    /// Completes pending output, then ends the session, within 450 ms (the 500 ms
    /// bound, 3.5): `VTCompressionSessionCompleteFrames` has no timeout, so it runs
    /// off the executor. When the bound passes it returns `.timedOut`: StreamCore then
    /// treats the session as hung and releases the permits of its frames still in
    /// flight (the sink's and the output handlers' references only keep the objects
    /// alive), so admission does not wait for VideoToolbox. Outputs that arrive later
    /// are stale.
    public func invalidate() -> InvalidateOutcome {
        guard let session = lock.withLock({ () -> VTCompressionSession? in
            defer { self.session = nil }
            return self.session
        }) else { return .completed }
        let done = DispatchSemaphore(value: 0)
        let ending = Ending(session)
        let completeFrames = completeFrames
        let deferred = deferred
        DispatchQueue.global(qos: .userInitiated).async {
            // Composites still waiting for their GPU work are encoded (or dropped) first.
            _ = deferred.wait(timeout: .now() + .milliseconds(200))
            completeFrames(ending.session)
            VTCompressionSessionInvalidate(ending.session)
            done.signal()
        }
        guard done.wait(timeout: .now() + Self.invalidateBound) == .success else {
            log?.error("StreamCore: VideoToolbox did not complete its frames within 450 ms")
            return .timedOut
        }
        return .completed
    }
}

/// Whether a VideoToolbox session runs on the hardware encoder. A low-latency session does
/// not report `UsingHardwareAcceleratedVideoEncoder` (`kVTPropertyNotSupportedErr`), so it
/// is read from the encoder the session uses (`EncoderID`): one `VTCopyVideoEncoderList`
/// marks hardware-accelerated, or the one VideoToolbox picks for the same codec and
/// low-latency rate control when hardware is required (Apple silicon's `…h264.rtvc`; the
/// software one is `…rtvc.sw`). Each codec's required-hardware encoder is looked up once.
public enum VideoToolboxHardware {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var required: [CMVideoCodecType: String?] = [:]

    public static func isHardware(_ session: VTCompressionSession, codec: CMVideoCodecType) -> Bool {
        var accelerated: CFTypeRef?
        let copied = withUnsafeMutablePointer(to: &accelerated) { pointer in
            VTSessionCopyProperty(session, key: kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
                                  allocator: nil, valueOut: UnsafeMutableRawPointer(pointer))
        }
        if copied == noErr { return (accelerated as? Bool) == true }
        guard let id = encoderID(session) else { return false }
        if listedHardware(id) { return true }
        return requiredHardwareEncoder(codec) == id
    }

    static func encoderID(_ session: VTCompressionSession) -> String? {
        var value: CFTypeRef?
        let status = withUnsafeMutablePointer(to: &value) { pointer in
            VTSessionCopyProperty(session, key: kVTCompressionPropertyKey_EncoderID, allocator: nil,
                                  valueOut: UnsafeMutableRawPointer(pointer))
        }
        return status == noErr ? value as? String : nil
    }

    private static func listedHardware(_ id: String) -> Bool {
        var list: CFArray?
        guard VTCopyVideoEncoderList(nil, &list) == noErr, let encoders = list as? [[String: Any]] else { return false }
        return encoders.contains { entry in
            entry[kVTVideoEncoderList_EncoderID as String] as? String == id
                && (entry[kVTVideoEncoderList_IsHardwareAccelerated as String] as? Bool) == true
        }
    }

    /// The encoder VideoToolbox uses for `codec` with low-latency rate control when
    /// hardware is required; nil without one (a VM, an Intel Mac without it).
    static func requiredHardwareEncoder(_ codec: CMVideoCodecType) -> String? {
        if let known = lock.withLock({ required[codec] }) { return known }
        let specification = [kVTVideoEncoderSpecification_EnableLowLatencyRateControl: true,
                             kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true] as CFDictionary
        var session: VTCompressionSession?
        let status = VTCompressionSessionCreate(allocator: nil, width: 640, height: 360, codecType: codec,
                                                encoderSpecification: specification, imageBufferAttributes: nil,
                                                compressedDataAllocator: nil, outputCallback: nil, refcon: nil,
                                                compressionSessionOut: &session)
        var id: String?
        if status == noErr, let session {
            id = encoderID(session)
            VTCompressionSessionInvalidate(session)
        }
        lock.withLock { required[codec] = .some(id) }
        return id
    }
}
#endif
