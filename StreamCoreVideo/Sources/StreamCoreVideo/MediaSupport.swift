// What the platform media code (StreamCoreAppleHost, the client modules) shares with the
// product above it: per-frame stage marks, FFI timing and logging, each optional. With nil
// there is no per-frame call.
import Dispatch
import Foundation

/// The native monotonic clock every stage time uses (nanoseconds; mach uptime on Apple).
public enum MediaClock {
    @inline(__always) public static func now() -> UInt64 { DispatchTime.now().uptimeNanoseconds }

    #if canImport(Darwin)
    /// Mach absolute-time ticks (such as ScreenCaptureKit's display time) in nanoseconds.
    public static func nanoseconds(machTicks ticks: UInt64) -> UInt64 {
        let (numer, denom) = timebase
        return UInt64((Double(ticks) * Double(numer) / Double(denom)).rounded())
    }
    private static let timebase: (UInt32, UInt32) = {
        var info = mach_timebase_info_data_t()
        mach_timebase_info(&info)
        return (info.numer, max(1, info.denom))
    }()
    #endif
}

/// What a submit of an image already submitted once is. StreamCore decides; the encoder
/// only sees the same image again.
public enum Resubmit: Sendable, Equatable {
    /// The image's first submit.
    case none
    /// A recovery: a forced keyframe or LTR refresh.
    case recovery
    /// The encoder skipped the newest image, so StreamCore encodes it again as a delta.
    case skipReencode
    /// A keep-warm repeat (design 6.3.1): the latest image again while input is recent
    /// and the screen is still.
    case keepWarm

    /// `again`: the image was submitted before.
    public init(again: Bool, opts: SubmitOpts) {
        self = !again ? .none
            : opts.keepWarmRepeat ? .keepWarm
            : opts.forceKeyframe || opts.ltrRefresh ? .recovery : .skipReencode
    }
}

/// Capture-side counts a trace may keep.
public enum FrameCounter: Sendable {
    case captured, statusIdle, statusOther
}

/// Per-frame host stage marks (design 9.2): H0 source present, H1 capture callback, H3
/// encoder submit, H4 encoder output, all on `MediaClock`. Called on the stream queue and
/// on encoder output threads; implementations must not block.
public protocol FrameTrace: AnyObject, Sendable {
    /// A submitted frame begins its record.
    func submitted(sequence: UInt64, h0: UInt64, h1: UInt64, h3: UInt64, resubmit: Resubmit)
    /// The encoder's output for `sequence` arrived.
    func encoded(sequence: UInt64, keyframe: Bool, h4: UInt64)
    /// The encoder produced no output for `sequence` (skip cause c).
    func encoderDrop(sequence: UInt64)
    /// A captured frame was replaced before it was submitted (skip cause b).
    func replaced(h0: UInt64, h1: UInt64)
    func count(_ counter: FrameCounter)
    func note(_ key: String, _ value: String)
}

/// Times the calls into StreamCore and the native work it calls back, per frame (the FFI
/// overhead gate). Each method runs `body` exactly once and returns its result.
public protocol FrameMeter: AnyObject, Sendable {
    /// A call into StreamCore on the stream queue; `drain` for a wake's run.
    func rust<T>(drain: Bool, _ body: () -> T) -> T
    /// Native work StreamCore called; `sequence` is the frame it submitted, `sent` when it
    /// handed that frame to the send path.
    func native<T>(sequence: UInt64?, sent: Bool, _ body: () -> T) -> T
    /// A call into StreamCore from an encoder output thread for `sequence`.
    func output<T>(sequence: UInt64, _ body: () -> T) -> T
}

/// The product's log. Messages are built only when a level is taken.
public protocol MediaLog: Sendable {
    func debug(_ message: @autoclosure () -> String)
    func info(_ message: @autoclosure () -> String)
    func error(_ message: @autoclosure () -> String)
}
