// macOS only: the host side (a Mac captures and encodes).
#if os(macOS)
import Foundation
import StreamCoreVideo

/// The stream's serial `DispatchQueue` as StreamCore's executor queue (3.5). `wake`
/// and `wakeAfter` only enqueue: they never run the drain inline, block or take a lock.
/// Wakes and drains go through StreamCore's per-frame C ABI once attached (`nativeWake`,
/// `StreamCoreNative.run`); timers stay on UniFFI.
public final class StreamSerialQueue: SerialQueue, @unchecked Sendable {
    private let queue: DispatchQueue
    private let port: StreamPort
    private let portKey: UInt64
    private let meter: (any FrameMeter)?

    public init(queue: DispatchQueue, port: StreamPort, meter: (any FrameMeter)?) {
        self.queue = queue; self.port = port; self.meter = meter
        portKey = port.nativeKey()
    }

    public func wake() {
        // Dispatch's own cost is the queue hop, not FFI.
        if let meter { meter.native(sequence: nil, sent: false, schedule) } else { schedule() }
    }

    private func schedule() {
        queue.async { [portKey, meter] in
            if let meter { meter.rust(drain: true) { StreamCoreNative.run(port: portKey) } } else { StreamCoreNative.run(port: portKey) }
        }
    }

    /// `wake` through the C ABI (the context is this queue, which the stream holds).
    public static let nativeWake: StreamCoreNative.WakeFunction = { queue in
        Unmanaged<StreamSerialQueue>.fromOpaque(queue!).takeUnretainedValue().wake()
    }

    public func wakeAfter(delayNs: UInt64, timer: UInt64) {
        let after = { [queue, port, meter] in
            queue.asyncAfter(deadline: .now() + .nanoseconds(Int(min(delayNs, UInt64(Int.max))))) {
                if let meter { meter.rust(drain: false) { port.runTimer(timer: timer) } } else { port.runTimer(timer: timer) }
            }
        }
        if let meter { meter.native(sequence: nil, sent: false, after) } else { after() }
    }
}

/// Observer jobs run as separate blocks on the stream queue, never inside a drain, so a
/// handler may call anything on the controller (3.5 "Consumer observers").
public final class StreamObserverQueue: ObserverQueue, @unchecked Sendable {
    private let queue: DispatchQueue
    private let handle: @Sendable (ObserverJob) -> Void

    public init(queue: DispatchQueue, handle: @escaping @Sendable (ObserverJob) -> Void) {
        self.queue = queue; self.handle = handle
    }

    public func post(job: ObserverJob) {
        queue.async { [handle] in handle(job) }
    }
}
#endif
