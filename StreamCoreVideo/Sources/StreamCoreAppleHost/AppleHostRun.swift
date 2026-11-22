// macOS only: the host side (a Mac captures and encodes).
#if os(macOS)
import AppKit
import CoreMedia
import Foundation
@preconcurrency import ScreenCaptureKit
import StreamCoreVideo

/// What one host run streams and how. The product decides every value: what a target
/// means in its interface, the size it settled on, the rate for its quality setting and
/// the switches it reads from its own settings.
public struct HostRunConfig: Sendable {
    public var streamID: UInt32
    /// The target StreamCore's plan names. The capture itself is the `SCStream` the
    /// product attaches (or, for a canvas, the member streams `HostCanvas` makes).
    public var target: CaptureTarget
    public var mode: StreamMode
    /// The codec asked for. HEVC that cannot be created falls back to H.264; the `started`
    /// job names the codec in use.
    public var codec: Codec
    public var width: Int
    public var height: Int
    public var fps: Int
    /// The starting bit rate, and the ceiling rate adaptation works under.
    public var bitrateBps: UInt64
    /// The frames.v4 extensions the client accepts, besides the layer and LTR ones the
    /// plan adds itself.
    public var extensions: [FrameExtension]
    /// LTR recovery: the client acknowledges LTR frames, and the product has it on. Used
    /// only where the encoder's probe confirms it for the codec.
    public var ltr: Bool
    /// Two temporal layers, on the same terms.
    public var temporalLayers: Bool
    public var pacing: PacingPolicy
    public var onSourceGone: OnSourceGone
    public var colorMapping: ColorMapping
    /// The display whose refresh rate paces admission (design 6.3); nil paces to the frame
    /// interval.
    public var refreshDisplay: CGDirectDisplayID?
    /// The captured content's size in points (the display, window or app the product's
    /// filter shows). With it, an update's `region` moves the capture live
    /// (`SCStreamConfiguration.sourceRect`, no restart); without, a region update is
    /// refused.
    public var contentSize: CGSize?
    /// How long the stream waits parked for another transport (8.8) before it fails with
    /// `stream.transport`, in nanoseconds; nil keeps StreamCore's default (3 s).
    public var parkNs: UInt64?

    public init(streamID: UInt32, target: CaptureTarget, mode: StreamMode, codec: Codec, width: Int, height: Int,
                fps: Int, bitrateBps: UInt64, extensions: [FrameExtension] = [], ltr: Bool = false,
                temporalLayers: Bool = false, pacing: PacingPolicy, onSourceGone: OnSourceGone = .fail,
                colorMapping: ColorMapping = ColorMapping(allowP3: true, srgbTransferAsBt709: false),
                refreshDisplay: CGDirectDisplayID? = nil, contentSize: CGSize? = nil, parkNs: UInt64? = nil) {
        self.streamID = streamID; self.target = target; self.mode = mode; self.codec = codec
        self.width = width; self.height = height; self.fps = fps; self.bitrateBps = bitrateBps
        self.extensions = extensions; self.ltr = ltr; self.temporalLayers = temporalLayers
        self.pacing = pacing; self.onSourceGone = onSourceGone; self.colorMapping = colorMapping
        self.refreshDisplay = refreshDisplay; self.contentSize = contentSize; self.parkNs = parkNs
    }
}

/// A combined canvas (design 3.9): the arrangement, the compositor and how member streams
/// are made (ScreenCaptureKit, or a test's stand-in).
public struct HostCanvas {
    public var arrangement: CanvasArrangement
    public var compositor: CanvasCompositorKind
    public var makeStream: CanvasCaptureBackend.StreamFactory
    public var observe: CanvasCaptureBackend.Observe
    /// The GPU compositor (tests make one that cannot start).
    public var makeMetal: (SurfaceStore) throws -> any Compositor

    public init(arrangement: CanvasArrangement, compositor: CanvasCompositorKind,
                makeStream: @escaping CanvasCaptureBackend.StreamFactory,
                observe: @escaping CanvasCaptureBackend.Observe = { DisplayLayoutObservation.current($0) },
                makeMetal: @escaping (SurfaceStore) throws -> any Compositor = { try MetalCanvasCompositor(surfaces: $0) }) {
        self.arrangement = arrangement; self.compositor = compositor; self.makeStream = makeStream
        self.observe = observe; self.makeMetal = makeMetal
    }
}

/// Why a run could not be made, besides StreamCore refusing the stream (a `MediaError`).
public enum HostRunError: Error, Sendable {
    /// The Metal canvas compositor could not start.
    case compositor(String)
}

/// One capture run on StreamCore's `StreamController`: the ScreenCaptureKit backend (or a
/// canvas's member streams and compositor), the VideoToolbox factory and the stream's
/// queues, with video sent by StreamCore's sink on `output`. A restart builds a new run.
///
/// The product builds the content filter and the `SCStream` with `backend` as its output
/// on `queue`, attaches it (`backend.attach`), then calls `start`. It hears the stream's
/// jobs (started, operation results, failures, closed) through `onJob`.
///
/// Threads: the executor drains on `queue` (capture callbacks drain inline); observer jobs
/// run as separate blocks on the same queue; VideoToolbox outputs and ScreenCaptureKit
/// completions only post. Call every method on `queue`, except `revoke`, `requestKeyframe`
/// and `noteInput`, which are safe from any thread.
public final class AppleHostRun: @unchecked Sendable {
    /// The single-target capture; nil on a canvas, whose members `canvasBackend` opens.
    public let backend: ScreenCaptureBackend?
    public let canvasBackend: CanvasCaptureBackend?
    public let compositor: (any Compositor)?
    public let factory: VideoToolboxEncoderFactory
    public let controller: StreamController
    public let surfaces: SurfaceStore
    /// What the plan uses of the encoder's layers and LTR (design 6.5), for the product to
    /// announce.
    public let planFeatures: EncoderPlanFeatures
    /// The pacing in use: the config's, with the maximum frame QP where the probe
    /// confirmed it.
    public let pacing: PacingPolicy
    /// The codec in use, once started.
    public private(set) var codec: Codec?
    /// How the stream closed, once it has.
    public private(set) var closed: CloseOutcome?
    /// Every job StreamCore posts for this stream, on `queue`.
    public var onJob: ((ObserverJob) -> Void)?
    /// The capture began (true) or stopped (false) delivering suspended frames: the Mac is
    /// locked, at the login window, or the display is asleep. On `queue`; single-target
    /// runs only.
    public var onSuspended: ((Bool) -> Void)? {
        get { backend?.onSuspended }
        set { backend?.onSuspended = newValue }
    }

    private let trace: (any FrameTrace)?
    private let meter: FFIMeter?
    private let log: (any MediaLog)?
    private let streamID: UInt32
    private let output: VideoOutput
    private var screenObserver: (any NSObjectProtocol)?

    private final class Box: @unchecked Sendable { weak var run: AppleHostRun? }

    /// The frame-level maximum QP rate adaptation gives a session whose probe confirmed
    /// it: H.264's and HEVC's largest, so VideoToolbox's own (lower) QP ceiling does not
    /// hold incompressible content far above the target.
    public static let maxFrameQp: UInt8 = 51

    /// The plan's encoder permit target (design 6.2). VideoToolbox's low-latency session
    /// outputs every frame before the next input, so with adaptive permits the base is 1
    /// and StreamCore adds one while encodes run slow; without, 3.
    public static func permitTarget(_ policy: PacingPolicy) -> UInt8 { policy.adaptivePermits ? 1 : 3 }

    /// `policy` with the maximum QP where the encoder's probe confirmed it.
    public static func withEncoderCaps(_ policy: PacingPolicy, _ caps: EncoderCaps?) -> PacingPolicy {
        var policy = policy
        policy.maxFrameQp = policy.rateAdaptation && caps?.maxFrameQp == true ? maxFrameQp : 0
        return policy
    }

    /// The plan's recovery steps and layer extensions: a feature the product has off, or
    /// the client does not take, counts as unconfirmed.
    public static func planFeatures(codec: Codec, ltr: Bool, temporalLayers: Bool,
                                    probe: (Codec) -> EncoderCaps = { VideoToolboxProbe.caps($0) }) -> EncoderPlanFeatures {
        guard ltr || temporalLayers else {
            return encoderPlanFeatures(caps: nil, clientLtr: false, clientTemporalLayers: false)
        }
        var caps = probe(codec)
        if !ltr, caps.ltr == .confirmed { caps.ltr = .requested }
        if !temporalLayers, caps.temporalLayers == .confirmed { caps.temporalLayers = .requested }
        return encoderPlanFeatures(caps: caps, clientLtr: ltr, clientTemporalLayers: temporalLayers)
    }

    /// Validates everything before any capture or encoder is allocated. Throws
    /// `HostRunError.compositor` when the Metal compositor cannot start, and a `MediaError`
    /// when StreamCore refuses the stream.
    ///
    /// - `configuration`: the `SCStream`'s configuration (frame interval, cursor, queue
    ///   depth). The run keeps it for live updates.
    /// - `output`: where the stream's video goes, e.g. `VideoOutput.channel`.
    /// - `authorize`: the product's authority over the source, asked on the encoder's
    ///   output thread before every output and so cheap and non-blocking. Once it returns
    ///   false the output is dropped and the stream's sink is revoked.
    /// - `trace`, `meter`, `log`: optional; with nil there is no per-frame call.
    public init(config: HostRunConfig, configuration: SCStreamConfiguration, queue: DispatchQueue, output: VideoOutput,
                canvas: HostCanvas? = nil, authorize: @escaping @Sendable () -> Bool = { true },
                trace: (any FrameTrace)? = nil, meter: FFIMeter? = nil, log: (any MediaLog)? = nil,
                sessionFactory: @escaping VideoToolboxSession.Factory = VideoToolboxSession.create) throws {
        if case .canvas = config.target, canvas == nil { throw MediaError.Unsupported }
        streamID = config.streamID
        self.trace = trace; self.meter = meter; self.log = log; self.output = output
        let surfaces = SurfaceStore()
        self.surfaces = surfaces
        if let canvas {
            backend = nil
            canvasBackend = CanvasCaptureBackend(surfaces: surfaces, wideGamut: canvas.compositor == .metal, trace: trace,
                                                 observe: canvas.observe, makeStream: canvas.makeStream)
            switch canvas.compositor {
            case .cpu: compositor = CPUCanvasCompositor(surfaces: surfaces)
            case .metal:
                do { compositor = try canvas.makeMetal(surfaces) } catch { throw HostRunError.compositor("\(error)") }
            }
        } else {
            backend = ScreenCaptureBackend(configuration: configuration, surfaces: surfaces, trace: trace, meter: meter,
                                           contentSize: config.contentSize)
            canvasBackend = nil; compositor = nil
        }
        // An output produced after the product's authority lapsed revokes the sink (a leaf
        // call) instead of reaching it.
        let revoker = Revoker()
        factory = VideoToolboxEncoderFactory(surfaces: surfaces, trace: trace, meter: meter, log: log,
                                             authorize: { [revoker] in
                                                 if authorize() { return true }
                                                 revoker.revoke()
                                                 return false
                                             },
                                             sessionFactory: sessionFactory)
        let interval = configuration.minimumFrameInterval
        let intervalNs = interval.isNumeric && interval.seconds > 0 ? UInt64(interval.seconds * 1_000_000_000) : 0
        // LTR and temporal layers only where the probe confirms them and the client takes them.
        let features = Self.planFeatures(codec: config.codec, ltr: config.ltr, temporalLayers: config.temporalLayers) {
            VideoToolboxProbe.caps($0, log: log)
        }
        planFeatures = features
        // Rate adaptation's maximum QP only where the probe confirmed it (H.264 is probed).
        let pacing = Self.withEncoderCaps(config.pacing, config.codec == .h264 ? VideoToolboxProbe.caps(.h264, log: log) : nil)
        self.pacing = pacing
        DisplayRefresh.shared.start()
        let refresh = config.refreshDisplay.flatMap { DisplayRefresh.shared.intervalNs(display: $0) }
        if pacing.refreshAligned, refresh == nil {
            log?.info("stream \(config.streamID): no display refresh interval; StreamCore paces to the frame interval")
        }
        // A canvas's members are BGRA at their tiles; the GPU path keeps one more buffer in
        // ScreenCaptureKit's pool than StreamCore leases (2 per member, 3.2).
        let spec = CaptureSpec(target: config.target, mode: config.mode, pixelWidth: UInt32(config.width),
                               pixelHeight: UInt32(config.height), minIntervalNs: intervalNs,
                               format: canvas == nil ? .nv12 : .bgra, color: .source,
                               cursor: configuration.showsCursor ? .composited : .hidden,
                               queueDepth: canvas.map { $0.compositor == .metal ? 4 : 3 }
                                   ?? UInt8(clamping: configuration.queueDepth), adapterLuid: nil)
        let plan = StreamPlan(mode: config.mode, codec: config.codec,
                              encodeWidth: UInt32(config.width), encodeHeight: UInt32(config.height), fps: UInt32(config.fps),
                              bitrateBps: config.bitrateBps,
                              convert: .identity, encoderInput: .nv12, encoder: "VideoToolbox",
                              // ScreenCaptureKit's usual NV12 tags; another source colour
                              // re-creates the session on the first frame (2.5.5). A canvas is
                              // sRGB (BT.709 primaries, sRGB transfer, BT.709 matrix): the
                              // compositor maps every member to it.
                              colorOut: ColorTags(primaries: .bt709, transfer: canvas == nil ? .bt709 : .srgb,
                                                  matrix: .bt709, fullRange: false),
                              colorMapping: config.colorMapping,
                              extensions: config.extensions + features.extensions,
                              permitTarget: Self.permitTarget(pacing), recovery: features.recovery,
                              onSourceGone: config.onSourceGone, mediumCapBps: nil, adapterLuid: nil,
                              // The display mode's refresh, never inferred from frame times (6.3).
                              refreshIntervalNs: refresh, pacing: pacing)
        let box = Box()
        let port = StreamPort()
        let serialQueue = StreamSerialQueue(queue: queue, port: port, meter: meter)
        let capture: CaptureBackend = canvasBackend ?? backend!
        let parts = StreamParts(
            streamId: config.streamID, spec: spec, plan: plan, port: port, queue: serialQueue,
            capture: capture, converter: nil, encoders: factory, output: output,
            observer: StreamObserverQueue(queue: queue) { job in box.run?.handle(job) },
            timings: config.parkNs.map { park in
                var timings = defaultStreamTimings()
                timings.parkNs = park
                return timings
            })
        if let canvas, let compositor {
            controller = try StreamController.newCanvas(parts: parts,
                                                        canvas: CanvasParts(layout: canvas.arrangement, compositor: compositor))
        } else {
            controller = try StreamController(parts: parts)
        }
        revoker.controller = controller
        // The per-frame calls (wake, frame, release) go through StreamCore's C ABI; the
        // stream holds each context (the queue, the backend) until Closed.
        if let canvasBackend {
            if StreamCoreNative.attach(port: port.nativeKey(), queue: serialQueue, wake: StreamSerialQueue.nativeWake,
                                       capture: canvasBackend, release: CanvasCaptureBackend.nativeRelease) {
                canvasBackend.nativePort = port.nativeKey()
            }
        } else if let backend,
                  StreamCoreNative.attach(port: port.nativeKey(), queue: serialQueue, wake: StreamSerialQueue.nativeWake,
                                          capture: backend, release: ScreenCaptureBackend.nativeRelease) {
            backend.nativePort = port.nativeKey()
        }
        box.run = self
        if let display = config.refreshDisplay, pacing.refreshAligned {
            // A display mode change (refresh rate included) reaches the pacer at once.
            screenObserver = NotificationCenter.default.addObserver(
                forName: NSApplication.didChangeScreenParametersNotification, object: nil, queue: .main
            ) { [weak controller] _ in
                MainActor.assumeIsolated { DisplayRefresh.shared.reload() }
                controller?.setRefreshInterval(refreshIntervalNs: DisplayRefresh.shared.intervalNs(display: display))
            }
        }
        if let trace {
            if let canvas { trace.note("sc_canvas_compositor", canvas.compositor.rawValue) }
            trace.note("pipeline", "streamcore")
            trace.note("sc_pacing", "refresh=\(pacing.refreshAligned) permits=\(pacing.adaptivePermits) recovery=\(pacing.immediateRecovery) abr=\(pacing.rateAdaptation) maxqp=\(pacing.maxFrameQp) keepwarm=\(pacing.keepWarmMs)")
            trace.note("sc_refresh_interval_ns", refresh.map { "\($0)" } ?? "unknown")
            trace.note("sc_layers", "recovery=\(features.recovery) extensions=\(features.extensions)")
            trace.note("encoded_size", "\(config.width)x\(config.height)")
            trace.note("max_fps", "\(config.fps)")
        }
    }

    deinit {
        if let screenObserver { NotificationCenter.default.removeObserver(screenObserver) }
    }

    // MARK: Operations

    /// Starts the capture; the result arrives as an `opResult` job for the returned token.
    @discardableResult
    public func start() -> OpToken { controller.start() }

    /// A live change: rate, frame interval, cursor, region, encode size. A region whose
    /// origin or size changes moves the capture with no restart (the product restarts for
    /// a new encode size). The result arrives as an `opResult` job for the returned token.
    @discardableResult
    public func update(_ update: StreamUpdate) -> OpToken { controller.update(update: update) }

    /// Begins closing; the `closed` job follows. After it, `closed` says how it went.
    public func beginClose() { controller.beginClose() }

    public func requestKeyframe() { controller.requestKeyframe() }

    /// Input reached the host for this stream: keep-warm's hold starts or extends (6.3.1).
    public func noteInput() { controller.noteInput() }

    /// Stops admission and sending at once (a leaf call, any thread).
    public func revoke() { controller.revoke() }

    /// The stream's transport is going away (8.8): the stream parks, keeping capture, its
    /// encoder session and the latest image, until `attachTransport` or `HostRunConfig.parkNs`
    /// (then it fails with `stream.transport`). A channel that closes parks the stream by
    /// itself once a send fails; this says so at once. A `transportParked` job follows.
    public func detachTransport() { controller.detachTransport() }

    /// Sends on `output` from now on, parked or not (8.8): video queued for the old transport
    /// is dropped, not counted as lost, and the next frame is a keyframe in the same wire
    /// generation, so the client keeps its decoder. `mediumCapBps` replaces the plan's
    /// medium cap for rate adaptation and the sink's capacity hint: nil means no cap, not
    /// "keep the old one", so every move states what the new medium allows. The result
    /// arrives as an `opResult` job for the returned token.
    @discardableResult
    public func attachTransport(_ output: VideoOutput, mediumCapBps: UInt64?) -> OpToken {
        controller.attachTransport(output: output, mediumCapBps: mediumCapBps)
    }

    /// StreamCore's counters as trace notes (`sc_…`), with the FFI overhead when metered.
    public func statsNotes() -> [(key: String, value: String)] {
        let stats = controller.stats()
        let counters: [(String, UInt64)] = [
            ("sc_frames_received", stats.framesReceived), ("sc_frames_replaced", stats.framesReplaced),
            ("sc_submitted", stats.submitted), ("sc_encoded", stats.encoded),
            ("sc_keep_warm_repeats", stats.keepWarmRepeats),
            ("sc_encoder_drops", stats.encoderDrops), ("sc_packets_refused", stats.packetsRefused),
            ("sc_recoveries", stats.recoveries), ("sc_encoders_created", stats.encodersCreated),
            ("sc_capture_skipped", stats.captureSkipped), ("sc_stale_events", stats.staleEvents),
            ("sc_sender_shed", stats.senderShed), ("sc_client_flushed", stats.clientFlushed),
            ("sc_pacer_held", stats.pacerHeld), ("sc_pacer_timer_admits", stats.pacerTimerAdmits),
            ("sc_permit_target", UInt64(stats.permitTarget)), ("sc_encode_p95_us", stats.encodeP95Us),
            ("sc_refresh_interval_ns", stats.refreshIntervalNs),
            ("sc_refresh_fallback", stats.refreshFallback ? 1 : 0),
            ("sc_packets_sent", stats.packetsSent),
            ("sc_sink_discardable_shed", stats.sinkDiscardableShed),
            ("sc_sink_superseded", stats.sinkSuperseded), ("sc_sink_aged_shed", stats.sinkAgedShed),
            ("sc_sink_held", stats.sinkHeld), ("sc_sink_unsent_max", UInt64(stats.sinkUnsentMax)),
            ("sc_credit_rtt_ms", UInt64(stats.creditRttMs)), ("sc_frame_timing_sent", stats.frameTimingSent),
            ("sc_frames_v4", stats.framesV4 ? 1 : 0), ("sc_video_bps", stats.videoBps),
            ("sc_bulk_capacity_bps", stats.bulkCapacityBps),
            ("sc_bulk_capacity_estimated", stats.bulkCapacityEstimated ? 1 : 0),
            ("sc_bulk_bucket_inactive", stats.bulkBucketInactive ? 1 : 0),
            ("sc_canvas_composed", stats.canvasComposed),
            ("sc_canvas_pool_exhausted", stats.canvasPoolExhausted),
            ("sc_canvas_member_frames", stats.canvasMemberFrames),
            ("sc_canvas_age_spread_max_us", stats.canvasAgeSpreadMaxUs),
            ("sc_abr_active", stats.abrActive ? 1 : 0), ("sc_abr_target_bps", stats.abrTargetBps),
            ("sc_abr_fps", UInt64(stats.abrFps)), ("sc_abr_queueing_us", stats.abrQueueingUs),
            ("sc_abr_decreases", stats.abrDecreases), ("sc_abr_tier_changes", stats.abrTierChanges),
            ("sc_client_feedback", stats.clientFeedback),
            ("sc_client_delay_samples", stats.clientDelaySamples), ("sc_client_lost", stats.clientLost),
            ("sc_bulk_video_bps", stats.bulkVideoBps), ("sc_link_rtt_us", stats.linkRttUs),
            ("sc_link_lost_packets", stats.linkLostPackets),
            ("sc_recoveries_on_ready", stats.recoveriesOnReady)]
        var notes = counters.map { (key: $0.0, value: "\($0.1)") }
        for (key, value) in (meter?.summary ?? [:]).sorted(by: { $0.key < $1.key }) { notes.append((key, value)) }
        return notes
    }

    /// Writes `statsNotes` to the run's trace, and the FFI overhead to its log.
    public func noteStats() {
        guard let trace else { return }
        for note in statsNotes() { trace.note(note.key, note.value) }
        if let meter, !meter.perFrame.cpu.isEmpty {
            let summary = meter.summary
            log?.info("stream \(streamID) StreamCore FFI overhead per frame (\(meter.perFrame.cpu.count) frames): CPU p50 \(summary["ffi_overhead_cpu_us_p50"] ?? "") µs, p99 \(summary["ffi_overhead_cpu_us_p99"] ?? "") µs; wall p50 \(summary["ffi_overhead_wall_us_p50"] ?? "") µs, p99 \(summary["ffi_overhead_wall_us_p99"] ?? "") µs")
        }
    }

    // MARK: Observer jobs (separate blocks on the stream queue)

    private func handle(_ job: ObserverJob) {
        switch job {
        case .started(_, let info):
            codec = info.codec
            trace?.note("codec", info.codec == .hevc ? "hevc" : "h264")
        case .closed(let outcome):
            closed = outcome
            noteStats()
        case .encoderFailed(let error):
            log?.error("stream \(streamID): StreamCore replaced a hung encoder session (\(String(describing: error)))")
        default:
            break
        }
        onJob?(job)
    }
}

/// Revokes the stream's sink from an encoder output thread once the product's authority
/// lapsed (`StreamController.revoke` is a leaf call).
private final class Revoker: @unchecked Sendable {
    private let lock = NSLock()
    private weak var _controller: StreamController?
    var controller: StreamController? {
        get { lock.withLock { _controller } }
        set { lock.withLock { _controller = newValue } }
    }
    func revoke() { controller?.revoke() }
}
#endif
