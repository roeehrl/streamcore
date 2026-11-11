//! The per-frame C ABI fast path (3.5 "Overhead"), declared in
//! `include/streamcore_video_native.h`.
//!
//! Through UniFFI, every call clones a handle, and every record, enum, option
//! and byte string is serialized into a buffer that the other side parses and
//! frees with further calls; a foreign trait callback also looks its object up
//! in a locked handle map. Between frames at 120 fps the caches are cold, and
//! the calls of one frame then cost a foreign backend tens of microseconds. A
//! foreign backend therefore makes the per-frame calls through this C ABI,
//! with primitives only, and keeps UniFFI for everything else:
//!
//! | Per frame | C ABI | UniFFI equivalent |
//! | --- | --- | --- |
//! | capture callback | [`streamcore_video_frame`] | `CaptureEvents::frame` |
//! | submit | `streamcore_video_submit_fn` | `StreamEncoder::submit` |
//! | surface release | `streamcore_video_stream_callbacks.release` | `CaptureBackend::release` |
//! | encoder output | [`streamcore_video_encoded`] | `submit_encoded` + `EncoderEvents::encoded` |
//! | wake | `streamcore_video_stream_callbacks.wake` | `SerialQueue::wake` |
//! | drain | [`streamcore_video_run`] | `StreamPort::run` |
//! | send | `streamcore_video_stream_callbacks.send` | `VideoOut::send` |
//! | send completion | [`streamcore_video_packet_complete`], [`streamcore_video_packet_release`] | `SinkReceipt::complete` |
//!
//! Native code names Rust objects by key ([`crate::StreamPort::native_key`],
//! [`crate::VideoSinkHandle::native_key`], [`Packet::key`]), never by pointer;
//! a key whose object is gone names nothing. Rust calls native code through
//! the function pointers attached with [`streamcore_video_attach_stream`] and
//! [`streamcore_video_attach_encoder`], passing back each one's context
//! unretained: the context is the very backend object given to the stream
//! through UniFFI, which the stream holds for as long as it may call it.
//!
//! [`streamcore_video_encoded`] is the crate's only `unsafe` code: it reads the
//! caller's output struct and byte ranges for the duration of the call. The
//! other exports need `allow(unsafe_code)` only for `no_mangle`.

use crate::error::MediaError;
use crate::executor::{SinkReceipt, StreamPort, VideoSinkHandle, lock};
use crate::types::{BitstreamForm, EncodeTimings, EncodedFrame, LtrInfo, VideoOutMeta};
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use streamcore_media::media::{MAX_FRAME_LENGTH, VIDEO_HEADER_LENGTH};

// ---------------------------------------------------------------------------
// Registries: native code names objects by key.

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

fn next_key() -> u64 {
    NEXT_KEY.fetch_add(1, Ordering::Relaxed)
}

static SINKS: Mutex<BTreeMap<u64, Weak<VideoSinkHandle>>> = Mutex::new(BTreeMap::new());
static PORTS: Mutex<BTreeMap<u64, Weak<StreamPort>>> = Mutex::new(BTreeMap::new());
static PACKETS: Mutex<BTreeMap<u64, Arc<NativePacket>>> = Mutex::new(BTreeMap::new());

pub(crate) fn register_sink(sink: Weak<VideoSinkHandle>) -> u64 {
    let key = next_key();
    lock(&SINKS).insert(key, sink);
    key
}

pub(crate) fn unregister_sink(key: u64) {
    lock(&SINKS).remove(&key);
}

fn sink(key: u64) -> Option<Arc<VideoSinkHandle>> {
    lock(&SINKS).get(&key)?.upgrade()
}

pub(crate) fn register_port(port: Weak<StreamPort>) -> u64 {
    let key = next_key();
    lock(&PORTS).insert(key, port);
    key
}

pub(crate) fn unregister_port(key: u64) {
    lock(&PORTS).remove(&key);
}

fn port(key: u64) -> Option<Arc<StreamPort>> {
    lock(&PORTS).get(&key)?.upgrade()
}

// ---------------------------------------------------------------------------
// Callbacks into native code.

/// `streamcore_video_stream_callbacks.wake`: `SerialQueue::wake`.
pub type WakeFn = extern "C" fn(queue: *mut c_void);
/// `streamcore_video_stream_callbacks.release`: `CaptureBackend::release`.
pub type ReleaseFn = extern "C" fn(capture: *mut c_void, surface: u64);
/// `streamcore_video_stream_callbacks.send`: `VideoOut::send`. `packet` is
/// valid until `streamcore_video_packet_release(packet->key)`.
pub type SendFn = extern "C" fn(out: *mut c_void, packet: *const Packet);
/// `streamcore_video_submit_fn`: `StreamEncoder::submit`, returning one of the
/// `SUBMIT_*` codes.
pub type SubmitFn = extern "C" fn(
    encoder: *mut c_void,
    surface: u64,
    submit_seq: u64,
    pts_ns: u64,
    capture_ns: u64,
    force_keyframe: bool,
) -> i32;

/// `streamcore_video_stream_callbacks`: the per-frame calls of a stream's
/// queue, capture backend and `VideoOut`. Each context is that backend object
/// as given in `StreamParts`, passed back unretained; a NULL function keeps
/// the UniFFI call.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct StreamCallbacks {
    pub queue: *mut c_void,
    pub wake: Option<WakeFn>,
    pub capture: *mut c_void,
    pub release: Option<ReleaseFn>,
    pub out: *mut c_void,
    pub send: Option<SendFn>,
}

/// A context pointer kept as an address, so the tables are `Send`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Context(usize);

impl Context {
    fn new(pointer: *mut c_void) -> Self {
        Self(pointer.expose_provenance())
    }

    fn get(self) -> *mut c_void {
        std::ptr::with_exposed_provenance_mut(self.0)
    }
}

/// The attached callbacks of one stream (none until attached; cleared at
/// `Closed`).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NativeStream {
    pub(crate) wake: Option<(Context, WakeFn)>,
    pub(crate) release: Option<(Context, ReleaseFn)>,
    pub(crate) send: Option<(Context, SendFn)>,
}

impl NativeStream {
    fn from_c(callbacks: StreamCallbacks) -> Self {
        Self {
            wake: callbacks.wake.map(|f| (Context::new(callbacks.queue), f)),
            release: callbacks
                .release
                .map(|f| (Context::new(callbacks.capture), f)),
            send: callbacks.send.map(|f| (Context::new(callbacks.out), f)),
        }
    }
}

pub(crate) fn wake(callback: (Context, WakeFn)) {
    (callback.1)(callback.0.get());
}

pub(crate) fn release(callback: (Context, ReleaseFn), surface: u64) {
    (callback.1)(callback.0.get(), surface);
}

/// An encoder's attached submit.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NativeSubmit(Context, SubmitFn);

/// `streamcore_video_submit_fn` results.
pub const SUBMITTED: i32 = 0;
pub const SUBMIT_BUSY: i32 = 1;
/// `Failed` with a `MediaError`: `SUBMIT_FAILED + ` [`media_error_code`].
pub const SUBMIT_FAILED: i32 = 16;

/// A `MediaError` as a small integer (the C ABI's error codes).
pub fn media_error_code(error: &MediaError) -> i32 {
    match error {
        MediaError::InvalidSettings => 0,
        MediaError::Unsupported => 1,
        MediaError::PermissionDenied => 2,
        MediaError::SourceGone => 3,
        MediaError::DeviceLost => 4,
        MediaError::OutOfResources => 5,
        MediaError::EncoderFailed { recoverable: false } => 6,
        MediaError::EncoderFailed { recoverable: true } => 7,
        MediaError::Timeout => 8,
        MediaError::Internal => 9,
    }
}

fn media_error(code: i32) -> MediaError {
    match code {
        0 => MediaError::InvalidSettings,
        1 => MediaError::Unsupported,
        2 => MediaError::PermissionDenied,
        3 => MediaError::SourceGone,
        4 => MediaError::DeviceLost,
        5 => MediaError::OutOfResources,
        6 => MediaError::EncoderFailed { recoverable: false },
        7 => MediaError::EncoderFailed { recoverable: true },
        8 => MediaError::Timeout,
        _ => MediaError::Internal,
    }
}

/// What a native submit returned.
pub(crate) enum Submitted {
    Ok,
    Busy,
    Failed(MediaError),
}

impl NativeSubmit {
    pub(crate) fn call(
        self,
        surface: u64,
        submit_seq: u64,
        pts_ns: u64,
        capture_ns: u64,
        force_keyframe: bool,
    ) -> Submitted {
        match (self.1)(
            self.0.get(),
            surface,
            submit_seq,
            pts_ns,
            capture_ns,
            force_keyframe,
        ) {
            SUBMITTED => Submitted::Ok,
            SUBMIT_BUSY => Submitted::Busy,
            code => Submitted::Failed(media_error(code - SUBMIT_FAILED)),
        }
    }
}

/// Attaches the per-frame callbacks of the stream whose `StreamPort` has key
/// `port`. Call it after creating the `StreamController` and before `start`;
/// returns false when no live stream has that port. `queue`, `capture` and
/// `out` must be the objects given as `StreamParts::queue`, `capture` and `out`
/// (each callback is called only while the stream holds that object).
#[allow(unsafe_code)] // `no_mangle`: an exported C symbol, prefixed `streamcore_video_`.
#[unsafe(no_mangle)]
pub extern "C" fn streamcore_video_attach_stream(
    port_key: u64,
    callbacks: StreamCallbacks,
) -> bool {
    match port(port_key).and_then(|port| port.executor()) {
        Some(executor) => executor.attach(NativeStream::from_c(callbacks)),
        None => false,
    }
}

/// Attaches `submit` for the encoder whose `EncoderSpec::sink` has key `sink`;
/// call it from `EncoderFactory::create` before returning `encoder`, which must
/// be the encoder object returned. The stream calls it instead of
/// `StreamEncoder::submit` while it holds that encoder, and keeps the frame's
/// permit itself until the frame's output or drop is posted (by either ABI).
#[allow(unsafe_code)] // `no_mangle`: an exported C symbol, prefixed `streamcore_video_`.
#[unsafe(no_mangle)]
pub extern "C" fn streamcore_video_attach_encoder(
    sink_key: u64,
    encoder: *mut c_void,
    submit: Option<SubmitFn>,
) -> bool {
    match (sink(sink_key), submit) {
        (Some(sink), Some(submit)) => {
            sink.attach(NativeSubmit(Context::new(encoder), submit));
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// In: capture frames and drains.

/// A `Complete` image of capture `capture` (single source) whose `SurfaceInfo`
/// equals the last `Complete` one `CaptureEvents::frame` delivered for that
/// capture except for `surface`, `capture_ns` and `source_present_ns` (`0`:
/// not reported), with no dirty rects and `GpuFence::None`. A backend uses
/// `frame` whenever kind, format, size, colour or rotation change. Returns false
/// when the port has no stream or no open capture: the surface was not taken
/// and the caller releases it. Otherwise the surface is leased to Rust (an
/// image that arrives before any `frame` is counted as invalid and released).
#[allow(unsafe_code)] // `no_mangle`: an exported C symbol, prefixed `streamcore_video_`.
#[unsafe(no_mangle)]
pub extern "C" fn streamcore_video_frame(
    port_key: u64,
    capture: u64,
    surface: u64,
    capture_ns: u64,
    source_present_ns: u64,
) -> bool {
    let Some(events) = port(port_key)
        .and_then(|port| port.executor())
        .and_then(|executor| executor.capture_port())
    else {
        return false;
    };
    events.frame_compact(capture, surface, capture_ns, source_present_ns);
    true
}

/// `StreamPort::run`: drain the stream's mailbox. Call only on the stream's
/// queue.
#[allow(unsafe_code)] // `no_mangle`: an exported C symbol, prefixed `streamcore_video_`.
#[unsafe(no_mangle)]
pub extern "C" fn streamcore_video_run(port_key: u64) {
    if let Some(port) = port(port_key) {
        port.run();
    }
}

// ---------------------------------------------------------------------------
// In: one encoder output.

/// `streamcore_video_encoded_output::flags`.
pub const ENCODED_KEYFRAME: u32 = 1;
pub const ENCODED_DISCARDABLE: u32 = 1 << 1;
/// The payload is Annex-B (the executor fails the stream on it, as on the
/// UniFFI path).
pub const ENCODED_ANNEX_B: u32 = 1 << 2;
/// `ltr_token` and `ltr_flags` are set.
pub const ENCODED_LTR: u32 = 1 << 3;

/// `streamcore_video_encoded_output`: an [`EncodedFrame`] with its bytes.
/// Times are host-monotonic nanoseconds, `0` when unknown.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EncodedOutput {
    pub submit_seq: u64,
    pub pts_ns: u64,
    /// H3, encoder input accepted.
    pub input_ns: u64,
    /// H4, access unit available.
    pub output_ns: u64,
    pub payload: *const u8,
    pub payload_len: usize,
    /// The codec configuration (avcC/hvcC) sent with this frame; NULL for none.
    pub config: *const u8,
    pub config_len: usize,
    pub flags: u32,
    pub ltr_token: u32,
    /// `-1`: none.
    pub temporal_id: i16,
    pub ltr_flags: u8,
}

/// Hands one encoder output to the stream: copies `payload` (and `config`) into
/// Rust once and posts the frame, from any thread, as `submit_encoded` followed
/// by `EncoderEvents::encoded` would. Returns false when the output was not
/// taken: an unknown or closed sink (a replaced encoder, whose outputs are stale
/// anyway) posts nothing; refused bytes (empty, over the frame limit, too many
/// waiting) post `dropped(submit_seq, NoOutput)`, so the caller posts nothing
/// more for the frame either way.
///
/// # Safety
///
/// `output` is NULL or points to a valid, aligned
/// `streamcore_video_encoded_output` for the duration of the call. Its
/// `payload` points to `payload_len` readable bytes, and its `config` is NULL or
/// points to `config_len` readable bytes, for the duration of the call; nothing
/// writes them meanwhile.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn streamcore_video_encoded(
    sink_key: u64,
    output: *const EncodedOutput,
) -> bool {
    if output.is_null() {
        return false;
    }
    // SAFETY: non-NULL, aligned and valid for the call by the caller's contract.
    let output = unsafe { *output };
    let Some(sink) = sink(sink_key) else {
        return false;
    };
    let limit = MAX_FRAME_LENGTH - VIDEO_HEADER_LENGTH;
    // An empty copy is refused by the sink (and posted as a drop).
    let copy = |bytes: *const u8, len: usize| -> Vec<u8> {
        if bytes.is_null() || len == 0 || len > limit {
            return Vec::new();
        }
        // SAFETY: non-NULL and `len` bytes readable for the call by the
        // caller's contract; `len` is at most the frame limit; copied before the
        // call returns.
        unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec()
    };
    let payload = copy(output.payload, output.payload_len);
    let config = (!output.config.is_null()).then(|| copy(output.config, output.config_len));
    let known = |ns: u64| (ns != 0).then_some(ns);
    let frame = EncodedFrame {
        submit_seq: output.submit_seq,
        pts_ns: output.pts_ns,
        keyframe: output.flags & ENCODED_KEYFRAME != 0,
        // Tokens are assigned by the sink.
        config_token: config.as_ref().map(|_| 0),
        payload_token: 0,
        form: if output.flags & ENCODED_ANNEX_B != 0 {
            BitstreamForm::AnnexB
        } else {
            BitstreamForm::Avcc4
        },
        discardable: output.flags & ENCODED_DISCARDABLE != 0,
        temporal_id: u8::try_from(output.temporal_id).ok(),
        ltr: (output.flags & ENCODED_LTR != 0).then_some(LtrInfo {
            token: output.ltr_token,
            flags: output.ltr_flags,
        }),
        timings: EncodeTimings {
            input_ns: known(output.input_ns),
            output_ns: known(output.output_ns),
        },
    };
    sink.encoded_native(frame, payload, config)
}

// ---------------------------------------------------------------------------
// Out: a packet described in place.

/// `streamcore_video_packet`: one packet handed to the attached `send`, its
/// metadata and bytes. Valid and unchanged until
/// `streamcore_video_packet_release(key)`. Flags are `0` or `1`; times are
/// host-monotonic nanoseconds, `0` when unknown.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet {
    /// Names the packet in `streamcore_video_packet_complete` and `_release`.
    pub key: u64,
    pub generation: u64,
    pub sequence: u64,
    pub pts_micros: u64,
    /// H1, the capture callback.
    pub capture_ns: u64,
    /// H0, the source present time.
    pub source_present_ns: u64,
    /// H3 and H4.
    pub encode_input_ns: u64,
    pub encode_output_ns: u64,
    /// `const uint8_t *`: the packet's bytes.
    pub bytes: usize,
    pub len: usize,
    pub stream_id: u32,
    /// The wire header's flags.
    pub flags: u8,
    pub droppable: u8,
    pub keyframe: u8,
    pub config: u8,
}

/// A packet lent to native code: its descriptor, its bytes and its receipt.
struct NativePacket {
    descriptor: Packet,
    _bytes: Vec<u8>,
    receipt: SinkReceipt,
}

/// Lends one packet to the attached `send`: it stays registered (its bytes
/// alive) until native code releases it.
pub(crate) fn send(
    callback: (Context, SendFn),
    meta: &VideoOutMeta,
    bytes: Vec<u8>,
    receipt: SinkReceipt,
) {
    let key = next_key();
    let timing = meta.timing;
    let descriptor = Packet {
        key,
        generation: meta.header.generation,
        sequence: meta.header.sequence,
        pts_micros: meta.header.pts_micros,
        capture_ns: timing.capture_ns,
        source_present_ns: timing.source_present_ns.unwrap_or(0),
        encode_input_ns: timing.encode.input_ns.unwrap_or(0),
        encode_output_ns: timing.encode.output_ns.unwrap_or(0),
        bytes: bytes.as_ptr().addr(),
        len: bytes.len(),
        stream_id: meta.header.stream_id,
        flags: meta.header.flags,
        droppable: u8::from(meta.droppable),
        keyframe: u8::from(meta.keyframe),
        config: u8::from(meta.config),
    };
    let packet = Arc::new(NativePacket {
        descriptor,
        _bytes: bytes,
        receipt,
    });
    let address = std::ptr::from_ref(&packet.descriptor);
    lock(&PACKETS).insert(key, packet);
    // No lock held: native code may complete and release the packet inside.
    (callback.1)(callback.0.get(), address);
}

/// The send's result (`SinkReceipt::complete`): true once the consumer's send
/// path took the packet, false when it refused or failed it. Only the first
/// call counts; any thread.
#[allow(unsafe_code)] // `no_mangle`: an exported C symbol, prefixed `streamcore_video_`.
#[unsafe(no_mangle)]
pub extern "C" fn streamcore_video_packet_complete(packet: u64, accepted: bool) {
    let packet = lock(&PACKETS).get(&packet).cloned();
    if let Some(packet) = packet {
        packet.receipt.complete(accepted);
    }
}

/// Ends the packet's loan: its descriptor and bytes are freed. A packet
/// released without completing counts as refused. Any thread.
#[allow(unsafe_code)] // `no_mangle`: an exported C symbol, prefixed `streamcore_video_`.
#[unsafe(no_mangle)]
pub extern "C" fn streamcore_video_packet_release(packet: u64) {
    let packet = lock(&PACKETS).remove(&packet);
    drop(packet);
}
