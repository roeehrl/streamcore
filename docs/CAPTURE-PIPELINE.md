# Native video admission, retention and recovery

StreamCore owns the portable rules for a video stream: how much capture and encode
work a stream may have in flight, what a native capture adapter must retain, how
generations, sequences and keyframes let a receiver recover, and which settings
and payload sizes are admissible. Pixel buffers, codec sessions and encoded bytes
stay native. Only small permits and header metadata cross the UniFFI boundary
(`streamcore-media/src/stream.rs`). A product's host implements the capture and
encoder side; how one product does so belongs in that product's documentation.

## Admission: `VideoWorkQueue`

Each stream owns one `VideoWorkQueue`. `acquire()` returns a `VideoWorkPermit`
while fewer than three permits are outstanding and the queue is open, and `None`
otherwise. The permit covers native encoding *and* the deferred handling of its
encoded output, including any later native queue hop, so an asynchronous encoder
cannot accumulate more than three frames of work per stream.

A native adapter must:

- Acquire before submitting a capture to the encoder. A saturated queue means the
  capture is not encoded now, so it never becomes a reference that later frames
  depend on.
- Release once output handling finishes, when the output is abandoned, or when
  the encode fails. `release()` and `Drop` are idempotent: releasing twice cannot
  create capacity.
- Reuse the same queue across encoder replacements, so callbacks from the old
  encoder still count until they are handled.
- Call `close()` when the stream stops. A closed queue admits nothing more.

## Latest-frame retention

The queue refuses work; it does not hold frames. A native adapter that sits behind
a capture source must therefore:

- Keep at most one latest raw capture waiting, replacing it with each newer
  arrival while admission is full, and submit it once a permit is free, even if the
  source has gone idle since.
- Keep that retained image so it can be re-encoded as a keyframe for recovery
  without waiting for another source update.
- Give every submission a strictly increasing submission sequence and timestamp,
  including a reused recovery image or a capture whose source time is older.
- Clear the retained image on stop or reconfiguration, and not retry a failed
  encode in an endless loop.

The capture source's own buffer pool is separate from these bounds; retaining one
frame holds one of its buffers.

## Generations, sequences and keyframes

`VideoPacketizer::next(keyframe, config_changed, sequence, pts)` is called once per
encoded sample, in encoder output order, before any congestion drop. It returns the
headers to send:

- The first sample, and any sample whose `config_changed` is true, starts a new
  generation. Generations are process-unique and increase across packetizers.
  A changed format (including a changed codec configuration atom) or a restarted
  encoder must report `config_changed`.
- A codec configuration header (flags 2, sequence 0) precedes the first sample of a
  generation and every keyframe, so a late or recovering receiver can configure.
- Late or duplicate submission sequences return nothing. Gaps left by encoder
  omission or congestion shedding are kept, never renumbered.

`VideoReceiver::accept` returns `Configure`, `Decode`, `Ignore` or `Recover`:

- Another stream ID or an older generation is ignored. A newer generation resets
  the receiver. Replaying a config within a configured generation is ignored, so it
  does not reset a healthy decoder.
- A sample before configuration, an invalid header, or a missing delta invalidates
  the reference chain; deltas return `Recover` until a keyframe arrives. A keyframe
  may skip sequences. Old or duplicate samples cannot repair a newer chain.
- `require_keyframe()` is for the native side's own invalidations (renderer flush,
  attachment, decode failure). `reject_config()` lets a repeated config be retried
  after a failed application.

`Recover` means the receiver should ask the sender for a keyframe through the
product's control protocol. The sender should answer by re-encoding the retained
image as a keyframe once send pressure eases, rate limited; a receiver should
likewise limit its requests. A keyframe submitted before the loss or request it is
meant to answer must not mark that recovery complete.

## Media bounds

`valid_video_settings` admits a maximum dimension of 2–8192 pixels, 1–120 frames per
second and a finite quality of 0–1. Products should validate both start and update
requests before touching capture or screen state, and still check actual
dimensions before any narrowing conversion or allocation. Platform codecs may
reject settings inside these limits.

Every encoded sample and codec configuration must fit the outer frame's 32-MiB
payload limit (`media::MAX_FRAME_LENGTH`) minus the 29-byte video header
(`media::VIDEO_HEADER_LENGTH`); see [BINARY.md](../protocol/BINARY.md). After
admission the [lane bounds](../protocol/LANES.md) apply. None of these limits is a
total-process-memory bound: capture, codec, TLS/QUIC and renderer allocations are
additional.

## Evidence

`crates/streamcore-media/tests/video_continuity.rs` covers invalid settings,
admission until output consumption, close, concurrent backends never exceeding
three permits, and the generation, sequence, config and keyframe rules above. Real
capture, encoder and renderer behaviour, frame rate, latency and soak results are
measured by the products that integrate StreamCore.
