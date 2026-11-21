import Foundation
import StreamCoreVideo

/// A serial `DispatchQueue` as a client stream's executor queue (design 3.5): `wake` and
/// `wakeAfter` only enqueue, never run the drain inline, block or take a lock. Give the
/// same `port` to `ClientParts`.
public final class ClientStreamQueue: SerialQueue, @unchecked Sendable {
    public let queue: DispatchQueue
    private let port: StreamPort

    public init(port: StreamPort, queue: DispatchQueue = DispatchQueue(label: "streamcore.video.client", qos: .userInteractive)) {
        self.port = port
        self.queue = queue
    }

    public func wake() {
        queue.async { [port] in port.run() }
    }

    public func wakeAfter(delayNs: UInt64, timer: UInt64) {
        queue.asyncAfter(deadline: .now() + .nanoseconds(Int(min(delayNs, UInt64(Int.max))))) { [port] in
            port.runTimer(timer: timer)
        }
    }
}
