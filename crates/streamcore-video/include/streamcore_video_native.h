// StreamCore video: the per-frame C ABI fast path (docs/STREAMCORE-VIDEO.md 3.5
// "Overhead", docs/VIDEO-THREADING.md, crates/streamcore-video/src/native.rs).
//
// A foreign backend makes the per-frame calls through these functions, with primitives
// only, and keeps the UniFFI bindings for everything else. Native code names Rust objects
// by key (StreamPort.nativeKey(), VideoSinkHandle.nativeKey(), streamcore_video_packet.key),
// never by pointer; a key whose object is gone names nothing. Rust calls native code through
// the attached function pointers, passing each one's context back unretained: the context
// must be the very backend object given to the stream through UniFFI, which the stream
// holds for as long as it may call that function. The threading rules of the UniFFI calls
// apply unchanged. Layouts are checked against the Rust definitions by tests/native.rs.
#ifndef STREAMCORE_VIDEO_NATIVE_H
#define STREAMCORE_VIDEO_NATIVE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// ---------------------------------------------------------------------------------------
// Out: Rust calls these instead of the UniFFI trait methods once attached.

// A packet handed to `send`: its metadata and bytes, valid and unchanged until
// streamcore_video_packet_release(key). The 0/1 fields are flags; times are host-monotonic
// nanoseconds, 0 when unknown.
typedef struct streamcore_video_packet {
    uint64_t key;
    uint64_t generation;
    uint64_t sequence;
    uint64_t pts_micros;
    uint64_t capture_ns;         // H1
    uint64_t source_present_ns;  // H0
    uint64_t encode_input_ns;    // H3
    uint64_t encode_output_ns;   // H4
    const uint8_t *bytes;
    size_t len;
    uint32_t stream_id;
    uint8_t flags;  // the wire header's flags
    uint8_t droppable;
    uint8_t keyframe;
    uint8_t config;
} streamcore_video_packet;

// The per-frame calls of a stream's SerialQueue (wake), CaptureBackend (release) and
// VideoOut (send). Each context is that object as given in StreamParts; a NULL function
// keeps the UniFFI call. `send` must complete and release the packet exactly once each
// (streamcore_video_packet_complete / _release), possibly inside the call.
typedef struct streamcore_video_stream_callbacks {
    void *queue;
    void (*wake)(void *queue);
    void *capture;
    void (*release)(void *capture, uint64_t surface);
    void *out;
    void (*send)(void *out, const streamcore_video_packet *packet);
} streamcore_video_stream_callbacks;

// Attaches the callbacks of the stream whose StreamPort has key `port`: after creating the
// StreamController, before start. False when no live stream has that port.
bool streamcore_video_attach_stream(uint64_t port, streamcore_video_stream_callbacks callbacks);

// StreamEncoder.submit with flat options. Returns STREAMCORE_VIDEO_SUBMITTED,
// STREAMCORE_VIDEO_SUBMIT_BUSY, or STREAMCORE_VIDEO_SUBMIT_FAILED plus a media error code.
// The stream keeps the frame's permit until its output or drop is posted (by either ABI).
#define STREAMCORE_VIDEO_SUBMITTED 0
#define STREAMCORE_VIDEO_SUBMIT_BUSY 1
#define STREAMCORE_VIDEO_SUBMIT_FAILED 16
// Media error codes (MediaError).
#define STREAMCORE_VIDEO_ERROR_INVALID_SETTINGS 0
#define STREAMCORE_VIDEO_ERROR_UNSUPPORTED 1
#define STREAMCORE_VIDEO_ERROR_PERMISSION_DENIED 2
#define STREAMCORE_VIDEO_ERROR_SOURCE_GONE 3
#define STREAMCORE_VIDEO_ERROR_DEVICE_LOST 4
#define STREAMCORE_VIDEO_ERROR_OUT_OF_RESOURCES 5
#define STREAMCORE_VIDEO_ERROR_ENCODER_FAILED 6
#define STREAMCORE_VIDEO_ERROR_ENCODER_FAILED_RECOVERABLE 7
#define STREAMCORE_VIDEO_ERROR_TIMEOUT 8
#define STREAMCORE_VIDEO_ERROR_INTERNAL 9
typedef int32_t (*streamcore_video_submit_fn)(void *encoder, uint64_t surface, uint64_t submit_seq,
                                               uint64_t pts_ns, uint64_t capture_ns, bool force_keyframe);

// Attaches `submit` for the encoder whose EncoderSpec.sink has key `sink`; call it from
// EncoderFactory.create before returning `encoder`, the encoder object returned.
bool streamcore_video_attach_encoder(uint64_t sink, void *encoder, streamcore_video_submit_fn submit);

// ---------------------------------------------------------------------------------------
// In.

// CaptureEvents.frame for a Complete single-source image shaped exactly like the last
// Complete frame CaptureEvents.frame delivered for `capture` (kind, format, size, colour,
// rotation), with no dirty rects and no fence; source_present_ns 0 when not reported. A
// backend calls CaptureEvents.frame whenever the shape changes. False when the port has no
// stream or no open capture: the surface was not taken and the caller releases it.
bool streamcore_video_frame(uint64_t port, uint64_t capture, uint64_t surface, uint64_t capture_ns,
                            uint64_t source_present_ns);

// StreamPort.run: drain the stream's mailbox. Only on the stream's queue.
void streamcore_video_run(uint64_t port);

// streamcore_video_encoded_output.flags
#define STREAMCORE_VIDEO_ENCODED_KEYFRAME (1u << 0)
#define STREAMCORE_VIDEO_ENCODED_DISCARDABLE (1u << 1)
// The payload is Annex-B (the stream fails on it, as on the UniFFI path).
#define STREAMCORE_VIDEO_ENCODED_ANNEX_B (1u << 2)
// ltr_token and ltr_flags are set.
#define STREAMCORE_VIDEO_ENCODED_LTR (1u << 3)

// One encoder output with its bytes. Times are host-monotonic nanoseconds, 0 when unknown.
typedef struct streamcore_video_encoded_output {
    uint64_t submit_seq;
    uint64_t pts_ns;
    uint64_t input_ns;   // H3, encoder input accepted
    uint64_t output_ns;  // H4, access unit available
    const uint8_t *payload;
    size_t payload_len;
    const uint8_t *config;  // avcC/hvcC sent with this frame; NULL for none
    size_t config_len;
    uint32_t flags;
    uint32_t ltr_token;
    int16_t temporal_id;  // -1: none
    uint8_t ltr_flags;
} streamcore_video_encoded_output;

// VideoSinkHandle.submitEncoded followed by EncoderEvents.encoded, from any thread: copies
// payload (and config) once and posts the frame. `sink` is VideoSinkHandle.nativeKey() of
// the encoder's EncoderSpec. False when the output was not taken: an unknown or closed sink
// (a replaced encoder) posts nothing; refused bytes post dropped(submit_seq, NoOutput).
// Either way the caller posts nothing more for the frame.
//
// `output` must point to a valid struct for the call; `payload` must point to `payload_len`
// readable bytes and `config` be NULL or point to `config_len` readable bytes, for the call.
bool streamcore_video_encoded(uint64_t sink, const streamcore_video_encoded_output *output);

// SinkReceipt.complete for a packet given to `send`: true once the consumer's send path
// took it, false when it refused or failed it. Only the first call counts; any thread.
void streamcore_video_packet_complete(uint64_t packet, bool accepted);

// Ends a packet's loan: its descriptor and bytes are freed. A packet released without
// completing counts as refused. Exactly once; any thread.
void streamcore_video_packet_release(uint64_t packet);

#ifdef __cplusplus
}
#endif

#endif
