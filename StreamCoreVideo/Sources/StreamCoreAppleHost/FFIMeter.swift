// macOS only: the host side (a Mac captures and encodes).
#if os(macOS)
import Foundation
import StreamCoreVideo

/// Time spent in Rust and the FFI per encoded frame (the gate: p99 ≤ 50 µs). Every
/// Rust entry on the stream queue (a capture callback's frame, a drain or a timer)
/// and every output-thread call (the encoder output's hand-over) is timed, minus the native
/// work Rust called during it on the same thread (submit, send, capture operations,
/// queue dispatch). Each entry's overhead is charged to the frame it submitted or
/// sent; a frame's total is recorded once it was handed to the send path. Both the
/// thread's CPU time (what the FFI and Rust cost, the gate's measure) and wall time
/// (which also counts preemption on a loaded machine) are kept.
public final class FFIMeter: FrameMeter, @unchecked Sendable {
    struct Time {
        var cpu: UInt64 = 0, wall: UInt64 = 0
        static func now() -> Time {
            Time(cpu: clock_gettime_nsec_np(CLOCK_THREAD_CPUTIME_ID), wall: clock_gettime_nsec_np(CLOCK_UPTIME_RAW))
        }
        static func &- (a: Time, b: Time) -> Time { Time(cpu: a.cpu &- b.cpu, wall: a.wall &- b.wall) }
        static func &+= (a: inout Time, b: Time) { a.cpu &+= b.cpu; a.wall &+= b.wall }
    }
    private struct Samples {
        var cpu: [UInt64] = [], wall: [UInt64] = []
        mutating func append(_ time: Time, capacity: Int) {
            if cpu.count >= capacity { cpu.removeFirst(cpu.count - capacity + 1); wall.removeFirst(wall.count - capacity + 1) }
            cpu.append(time.cpu); wall.append(time.wall)
        }
    }
    private struct State {
        var open: [UInt64: Time] = [:]
        var frames = Samples()
        var unattributed = Samples()
        /// Per-part samples: queue entries that submitted a frame (a capture callback's
        /// inline drain), that sent one (the output drain), and output-thread calls.
        var submits = Samples(), sends = Samples(), outputs = Samples()
        var recorded = 0
        /// Frames whose output was handed to Rust: the next drain pushes them to the sink.
        var awaitingSink: [UInt64] = []
    }
    private let lock = NSLock()
    private var state = State()
    private let capacity = 8192
    private let publish: (@Sendable ([String: String]) -> Void)?

    public init(publish: (@Sendable ([String: String]) -> Void)? = nil) { self.publish = publish }

    /// The Rust entry the current thread is in. Kept per thread (a pthread-specific slot)
    /// and touched without a lock, so between its clock reads the meter does nothing but
    /// read clocks: its own bookkeeping (locks, dictionaries) stays out of what it measures.
    private final class Entry {
        var active = false
        /// How deep the thread is in native work (`native` brackets) of this entry.
        var depth = 0
        /// Wall time of the outermost native work Rust called during the entry, and of the
        /// calls into Rust made from inside that native work (added back).
        var native: UInt64 = 0, reentered: UInt64 = 0
        /// The first frame the entry submitted or sent, and the frames it sent.
        var first: UInt64?
        var sent: [UInt64] = []
        init() { sent.reserveCapacity(4) }
        func begin() {
            active = true; depth = 0; native = 0; reentered = 0; first = nil; sent.removeAll(keepingCapacity: true)
        }
        /// The entry's overhead: its CPU time minus the native work in it. Native work is
        /// timed with the wall clock (a commpage read; the thread CPU clock is a kernel call
        /// costing ~0.6 µs, ~1.7 µs at p99, when cold, which the meter would otherwise count
        /// several times per frame), outermost brackets only, minus the calls into Rust made
        /// inside them (`rust` or `output` reached from native work). The thread's CPU time
        /// equals its wall time while it runs, so the entry's preemption is wall − CPU; at
        /// most that much of the native wall time can be preemption, so the native CPU time
        /// subtracted is at least native − preemption. Preemption therefore never makes it
        /// under-report; a call into Rust from native work that bypasses `rust`/`output`
        /// would. In a VM the guest's CPU clock includes host steal time.
        func overhead(_ elapsed: Time) -> Time {
            let net = native > reentered ? native - reentered : 0
            let preempted = elapsed.wall > elapsed.cpu ? elapsed.wall - elapsed.cpu : 0
            let nativeCPU = net > preempted ? net - preempted : 0
            return Time(cpu: elapsed.cpu > nativeCPU ? elapsed.cpu - nativeCPU : 0,
                        wall: elapsed.wall > net ? elapsed.wall - net : 0)
        }
        /// A call into Rust while this entry is active: inside native work its time is
        /// added back to the entry; directly inside the entry it is already counted.
        func nested<T>(_ body: () -> T) -> T {
            guard depth > 0 else { return body() }
            let start = clock_gettime_nsec_np(CLOCK_UPTIME_RAW)
            let result = body()
            reentered &+= clock_gettime_nsec_np(CLOCK_UPTIME_RAW) &- start
            return result
        }
    }

    private static let entryKey: pthread_key_t = {
        var key = pthread_key_t()
        pthread_key_create(&key) { Unmanaged<AnyObject>.fromOpaque($0).release() }
        return key
    }()

    private static var entry: Entry {
        if let raw = pthread_getspecific(entryKey) { return Unmanaged<Entry>.fromOpaque(raw).takeUnretainedValue() }
        let entry = Entry()
        pthread_setspecific(entryKey, Unmanaged.passRetained(entry).toOpaque())
        return entry
    }

    /// A Rust entry on the stream queue. Inside another entry it counts toward that one. A
    /// `drain` (a wake's `run`, not a capture callback's inline frame) handles the posted
    /// outputs: it pushes them to StreamCore's sink, which ends their frames.
    @discardableResult
    public func rust<T>(drain: Bool = false, _ body: () -> T) -> T {
        let entry = Self.entry
        if entry.active { return entry.nested(body) }
        entry.begin()
        let start = Time.now()
        let result = body()
        let elapsed = Time.now() &- start
        entry.active = false
        let overhead = entry.overhead(elapsed)
        var notes: [String: String]?
        lock.withLock {
            if drain, !state.awaitingSink.isEmpty {
                for sequence in state.awaitingSink where !entry.sent.contains(sequence) { entry.sent.append(sequence) }
                state.awaitingSink.removeAll(keepingCapacity: true)
                if entry.first == nil { entry.first = entry.sent.first }
            }
            if let first = entry.first {
                state.open[first, default: Time()] &+= overhead
                if entry.sent.isEmpty { state.submits.append(overhead, capacity: capacity) }
                else { state.sends.append(overhead, capacity: capacity) }
            } else {
                state.unattributed.append(overhead, capacity: capacity)
            }
            for sequence in entry.sent {
                if let total = state.open.removeValue(forKey: sequence) { state.frames.append(total, capacity: capacity); state.recorded += 1 }
            }
            if state.open.count > 64 { for key in state.open.keys.sorted().prefix(state.open.count - 64) { state.open.removeValue(forKey: key) } }
            if !entry.sent.isEmpty, state.recorded % 64 == 0, publish != nil { notes = summaryLocked() }
        }
        if let notes { publish?(notes) }
        return result
    }

    /// Native work Rust called during the current entry; `sequence` is the frame it
    /// submitted (`sent` when it handed that frame to the send path). Outside an entry it
    /// only runs `body`.
    public func native<T>(sequence: UInt64? = nil, sent: Bool = false, _ body: () -> T) -> T {
        let entry = Self.entry
        guard entry.active else { return body() }
        entry.depth += 1
        let outermost = entry.depth == 1
        let start = outermost ? clock_gettime_nsec_np(CLOCK_UPTIME_RAW) : 0
        let result = body()
        if outermost { entry.native &+= clock_gettime_nsec_np(CLOCK_UPTIME_RAW) &- start }
        entry.depth -= 1
        if let sequence {
            if entry.first == nil { entry.first = sequence }
            if sent, !entry.sent.contains(sequence) { entry.sent.append(sequence) }
        }
        return result
    }

    /// Calls into Rust from an encoder output thread for `sequence`. Inside another entry
    /// (an output delivered synchronously inside `submit`) it counts toward that one.
    public func output<T>(sequence: UInt64, _ body: () -> T) -> T {
        let entry = Self.entry
        if entry.active { return entry.nested(body) }
        entry.begin()
        let start = Time.now()
        let result = body()
        let elapsed = Time.now() &- start
        entry.active = false
        let overhead = entry.overhead(elapsed)
        lock.withLock {
            state.open[sequence, default: Time()] &+= overhead
            state.outputs.append(overhead, capacity: capacity)
            state.awaitingSink.append(sequence)
            if state.awaitingSink.count > 64 { state.awaitingSink.removeFirst(state.awaitingSink.count - 64) }
        }
        return result
    }

    /// p50/p99 (µs) of per-frame overhead (CPU and wall) and of its parts.
    public var summary: [String: String] { lock.withLock { summaryLocked() } }

    /// Per-frame overhead in thread CPU ns, and in wall ns.
    public var perFrame: (cpu: [UInt64], wall: [UInt64]) { lock.withLock { (state.frames.cpu, state.frames.wall) } }

    private func summaryLocked() -> [String: String] {
        func percentile(_ values: [UInt64], _ p: Double) -> String {
            guard !values.isEmpty else { return "" }
            let sorted = values.sorted()
            let index = min(sorted.count - 1, max(0, Int((p * Double(sorted.count)).rounded(.up)) - 1))
            return String(format: "%.1f", Double(sorted[index]) / 1000)
        }
        var notes = ["ffi_frames": "\(state.frames.cpu.count)",
                     "ffi_unattributed_entries": "\(state.unattributed.cpu.count)"]
        for (name, samples) in [("overhead", state.frames), ("submit_entry", state.submits), ("send_entry", state.sends),
                                ("output", state.outputs), ("unattributed", state.unattributed)] {
            for (clock, values) in [("cpu", samples.cpu), ("wall", samples.wall)] {
                notes["ffi_\(name)_\(clock)_us_p50"] = percentile(values, 0.5)
                notes["ffi_\(name)_\(clock)_us_p99"] = percentile(values, 0.99)
            }
        }
        notes["ffi_overhead_cpu_us_max"] = percentile(state.frames.cpu, 1)
        notes["ffi_overhead_wall_us_max"] = percentile(state.frames.wall, 1)
        return notes
    }
}
#endif
