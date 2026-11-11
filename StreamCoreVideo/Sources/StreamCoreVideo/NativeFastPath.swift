// The per-frame C ABI fast path for Swift adapters (docs/STREAMCORE-VIDEO.md 3.5 "Overhead",
// docs/VIDEO-THREADING.md, crates/streamcore-video/include/streamcore_video_native.h): the calls
// a frame makes, with primitives only, keeping the UniFFI bindings for everything else.
//
// Attach the callbacks once (`attach(port:...)` after creating the StreamController, before
// `start`; `attachEncoder` inside `EncoderFactory.create`). Their contexts are the very objects
// given to the stream through UniFFI, passed back unretained; the C functions are written per
// concrete type, for example
//
//     let wake: StreamCoreNative.WakeFunction = { Unmanaged<MyQueue>.fromOpaque($0!).takeUnretainedValue().wake() }
//
// A product that builds the bindings into its own C module copies this file beside them and
// renames the module import, as UniFFI's `ffi_module_name` does for the generated files.
import Foundation
#if canImport(streamcore_videoFFI)
import streamcore_videoFFI
#endif

public enum StreamCoreNative {
    public typealias WakeFunction = @convention(c) (UnsafeMutableRawPointer?) -> Void
    public typealias ReleaseFunction = @convention(c) (UnsafeMutableRawPointer?, UInt64) -> Void
    public typealias SendFunction = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<streamcore_video_packet>?) -> Void
    public typealias SubmitFunction = @convention(c) (UnsafeMutableRawPointer?, UInt64, UInt64, UInt64, UInt64, Bool) -> Int32

    /// Attaches the per-frame calls of the stream whose port has key `port`
    /// (`StreamPort.nativeKey()`): `queue`, `capture` and `out` are the objects given as
    /// `StreamParts.queue`, `.capture` and `.out`; a nil function keeps the UniFFI call.
    @discardableResult
    public static func attach(port: UInt64, queue: AnyObject, wake: WakeFunction?,
                              capture: AnyObject, release: ReleaseFunction?,
                              out: AnyObject, send: SendFunction?) -> Bool {
        streamcore_video_attach_stream(port, streamcore_video_stream_callbacks(
            queue: Unmanaged.passUnretained(queue).toOpaque(), wake: wake,
            capture: Unmanaged.passUnretained(capture).toOpaque(), release: release,
            out: Unmanaged.passUnretained(out).toOpaque(), send: send))
    }

    /// Attaches `submit` for the encoder being created (`spec.sink.nativeKey()`); call it in
    /// `EncoderFactory.create` with the encoder object returned. The function returns
    /// `submitted`, `busy` or `failed(_:)`.
    @discardableResult
    public static func attachEncoder(sink: UInt64, encoder: AnyObject, submit: SubmitFunction) -> Bool {
        streamcore_video_attach_encoder(sink, Unmanaged.passUnretained(encoder).toOpaque(), submit)
    }

    /// `streamcore_video_submit_fn` results.
    public static let submitted = Int32(STREAMCORE_VIDEO_SUBMITTED)
    public static let busy = Int32(STREAMCORE_VIDEO_SUBMIT_BUSY)
    public static func failed(_ error: MediaError) -> Int32 {
        let code: Int32 = switch error {
        case .InvalidSettings: STREAMCORE_VIDEO_ERROR_INVALID_SETTINGS
        case .Unsupported: STREAMCORE_VIDEO_ERROR_UNSUPPORTED
        case .PermissionDenied: STREAMCORE_VIDEO_ERROR_PERMISSION_DENIED
        case .SourceGone: STREAMCORE_VIDEO_ERROR_SOURCE_GONE
        case .DeviceLost: STREAMCORE_VIDEO_ERROR_DEVICE_LOST
        case .OutOfResources: STREAMCORE_VIDEO_ERROR_OUT_OF_RESOURCES
        case .EncoderFailed(let recoverable):
            recoverable ? STREAMCORE_VIDEO_ERROR_ENCODER_FAILED_RECOVERABLE : STREAMCORE_VIDEO_ERROR_ENCODER_FAILED
        case .Timeout: STREAMCORE_VIDEO_ERROR_TIMEOUT
        case .Internal: STREAMCORE_VIDEO_ERROR_INTERNAL
        }
        return Int32(STREAMCORE_VIDEO_SUBMIT_FAILED) + code
    }

    /// `CaptureEvents.frame` for a complete image shaped like the last full `frame` of
    /// `capture` (`sourcePresentNs` 0 when unknown). False when the surface was not taken (no
    /// stream, no open capture, or no full frame of this capture yet, as after a restart):
    /// deliver it with the full `frame` then.
    public static func frame(port: UInt64, capture: UInt64, surface: UInt64, captureNs: UInt64,
                             sourcePresentNs: UInt64) -> Bool {
        streamcore_video_frame(port, capture, surface, captureNs, sourcePresentNs)
    }

    /// `SinkReceipt.complete` for packet `key` (`NativeVideoPacket.complete`).
    public static func complete(packet key: UInt64, accepted: Bool) { streamcore_video_packet_complete(key, accepted) }

    /// `StreamPort.run`, on the stream's queue.
    public static func run(port: UInt64) { streamcore_video_run(port) }

    /// Hands one encoder output to its stream from any thread: `payload` (and `config`, the
    /// avcC/hvcC sent with this frame) are copied into Rust once, then the frame is posted,
    /// as `submitEncoded` followed by `EncoderEvents.encoded` would. False when the output
    /// was not taken; the stream then already counts the frame as dropped (or the encoder
    /// was replaced), so the caller posts nothing more for it.
    public static func encoded(sink: UInt64, _ output: NativeEncodedOutput, payload: UnsafeRawBufferPointer,
                               config: UnsafeRawBufferPointer? = nil) -> Bool {
        var flags: UInt32 = 0
        if output.keyframe { flags |= STREAMCORE_VIDEO_ENCODED_KEYFRAME }
        if output.discardable { flags |= STREAMCORE_VIDEO_ENCODED_DISCARDABLE }
        if output.annexB { flags |= STREAMCORE_VIDEO_ENCODED_ANNEX_B }
        if output.ltr != nil { flags |= STREAMCORE_VIDEO_ENCODED_LTR }
        var native = streamcore_video_encoded_output(
            submit_seq: output.submitSeq, pts_ns: output.ptsNs, input_ns: output.inputNs,
            output_ns: output.outputNs,
            payload: payload.baseAddress?.assumingMemoryBound(to: UInt8.self), payload_len: payload.count,
            config: config?.baseAddress?.assumingMemoryBound(to: UInt8.self), config_len: config?.count ?? 0,
            flags: flags, ltr_token: output.ltr?.token ?? 0, temporal_id: output.temporalId.map(Int16.init) ?? -1,
            ltr_flags: output.ltr?.flags ?? 0)
        return streamcore_video_encoded(sink, &native)
    }
}

/// One encoder output for `StreamCoreNative.encoded` (an `EncodedFrame` without tokens).
/// Times are host-monotonic nanoseconds, 0 when unknown.
public struct NativeEncodedOutput {
    public var submitSeq: UInt64
    public var ptsNs: UInt64
    public var keyframe: Bool
    /// H3 and H4.
    public var inputNs: UInt64
    public var outputNs: UInt64
    public var discardable = false
    public var annexB = false
    public var temporalId: UInt8?
    public var ltr: LtrInfo?

    public init(submitSeq: UInt64, ptsNs: UInt64, keyframe: Bool, inputNs: UInt64, outputNs: UInt64) {
        self.submitSeq = submitSeq; self.ptsNs = ptsNs; self.keyframe = keyframe
        self.inputNs = inputNs; self.outputNs = outputNs
    }
}

/// A packet handed to the attached `send`: its metadata and its bytes, lent until
/// `release()`. Complete it once (`complete(accepted:)`) and release it once, in either
/// order: release directly, or by the `Data` from `adoptData()` when it is freed. Times are
/// host-monotonic nanoseconds, nil when unknown.
public struct NativeVideoPacket: @unchecked Sendable {
    public let key: UInt64
    public let streamId: UInt32
    public let generation: UInt64
    public let sequence: UInt64
    /// The wire header's flags.
    public let flags: UInt8
    public let ptsMicros: UInt64
    public let droppable: Bool
    public let keyframe: Bool
    public let config: Bool
    /// H0, H1, H3 and H4.
    public let sourcePresentNs: UInt64?
    public let captureNs: UInt64
    public let encodeInputNs: UInt64?
    public let encodeOutputNs: UInt64?
    /// Valid until the packet is released.
    public let bytes: UnsafeRawBufferPointer

    public init(_ packet: UnsafePointer<streamcore_video_packet>) {
        let p = packet.pointee
        func known(_ value: UInt64) -> UInt64? { value == 0 ? nil : value }
        key = p.key; streamId = p.stream_id; generation = p.generation; sequence = p.sequence
        flags = p.flags; ptsMicros = p.pts_micros
        droppable = p.droppable != 0; keyframe = p.keyframe != 0; config = p.config != 0
        sourcePresentNs = known(p.source_present_ns); captureNs = p.capture_ns
        encodeInputNs = known(p.encode_input_ns); encodeOutputNs = known(p.encode_output_ns)
        bytes = UnsafeRawBufferPointer(start: p.bytes, count: p.len)
    }

    /// The send's result (`SinkReceipt.complete`); only the first call counts.
    public func complete(accepted: Bool) { streamcore_video_packet_complete(key, accepted) }

    /// Ends the loan of the bytes (it does not complete the send).
    public func release() { streamcore_video_packet_release(key) }

    /// The bytes as `Data` without a copy; freeing the `Data` releases the packet, so do
    /// not also call `release()`. The bytes are the `Data`'s until then.
    public func adoptData() -> Data {
        let key = self.key
        guard let base = bytes.baseAddress, bytes.count > 0 else { release(); return Data() }
        return Data(bytesNoCopy: UnsafeMutableRawPointer(mutating: base), count: bytes.count,
                    deallocator: .custom { _, _ in streamcore_video_packet_release(key) })
    }
}
