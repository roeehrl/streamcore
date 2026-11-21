// macOS only: the host side (a Mac captures and encodes).
#if os(macOS)
import AppKit
import CoreGraphics
import Foundation
import Synchronization

/// Display refresh intervals for StreamCore's refresh-aligned admission (design 6.3),
/// taken from the display mode, never from frame times: `NSScreen.maximumFramesPerSecond`
/// (read on the main thread, kept current on screen parameter changes), else the
/// CoreGraphics display mode's refresh rate, else unknown (StreamCore then aligns to the
/// frame interval).
public final class DisplayRefresh: Sendable {
    public static let shared = DisplayRefresh()
    private let table = Mutex<[CGDirectDisplayID: UInt64]>([:])
    private let observing = Mutex(false)

    /// Starts tracking `NSScreen` (once); safe from any thread.
    public func start() {
        let first = observing.withLock { observing -> Bool in
            defer { observing = true }
            return !observing
        }
        guard first else { return }
        DispatchQueue.main.async {
            NotificationCenter.default.addObserver(forName: NSApplication.didChangeScreenParametersNotification,
                                                   object: nil, queue: .main) { _ in
                MainActor.assumeIsolated { DisplayRefresh.shared.reload() }
            }
            MainActor.assumeIsolated { self.reload() }
        }
    }

    /// Re-reads every screen's refresh rate.
    @MainActor public func reload() {
        var intervals: [CGDirectDisplayID: UInt64] = [:]
        for screen in NSScreen.screens {
            guard let number = screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber,
                  let interval = Self.interval(fps: Double(screen.maximumFramesPerSecond)) else { continue }
            intervals[number.uint32Value] = interval
        }
        table.withLock { $0 = intervals }
    }

    /// The display's refresh interval in nanoseconds, or nil when unknown.
    public func intervalNs(display: CGDirectDisplayID) -> UInt64? {
        if let known = table.withLock({ $0[display] }) { return known }
        guard let mode = CGDisplayCopyDisplayMode(display) else { return nil }
        return Self.interval(fps: mode.refreshRate)
    }

    /// `1 s / fps` for a plausible panel rate (1–1000 Hz), else nil.
    public static func interval(fps: Double) -> UInt64? {
        guard fps.isFinite, fps >= 1, fps <= 1_000 else { return nil }
        return UInt64((1_000_000_000 / fps).rounded())
    }

    /// The display showing most of `rect` (global display coordinates), if any.
    public static func display(containing rect: CGRect) -> CGDirectDisplayID? {
        guard !rect.isNull, !rect.isEmpty else { return nil }
        var displays = [CGDirectDisplayID](repeating: 0, count: 16)
        var count: UInt32 = 0
        guard CGGetDisplaysWithRect(rect, UInt32(displays.count), &displays, &count) == .success, count > 0 else { return nil }
        return displays.prefix(Int(count)).max { a, b in
            CGDisplayBounds(a).intersection(rect).area < CGDisplayBounds(b).intersection(rect).area
        }
    }
}

private extension CGRect {
    var area: CGFloat { isNull ? 0 : width * height }
}
#endif
