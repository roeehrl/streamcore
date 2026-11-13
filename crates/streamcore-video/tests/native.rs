//! The per-frame C ABI (3.5 "Overhead", `include/streamcore_video_native.h`):
//! its layouts, a stream whose backends make every per-frame call through it,
//! mixed with the UniFFI calls, and the wakes a frame costs.

mod support;

use std::mem::{offset_of, size_of};
use std::sync::atomic::Ordering;
use streamcore_video::native::*;
use streamcore_video::*;
use support::*;

#[test]
fn c_layouts_match_the_header() {
    // include/streamcore_video_native.h on a 64-bit target.
    assert_eq!(size_of::<EncodedOutput>(), 80);
    assert_eq!(offset_of!(EncodedOutput, payload), 32);
    assert_eq!(offset_of!(EncodedOutput, payload_len), 40);
    assert_eq!(offset_of!(EncodedOutput, config), 48);
    assert_eq!(offset_of!(EncodedOutput, config_len), 56);
    assert_eq!(offset_of!(EncodedOutput, flags), 64);
    assert_eq!(offset_of!(EncodedOutput, ltr_token), 68);
    assert_eq!(offset_of!(EncodedOutput, temporal_id), 72);
    assert_eq!(offset_of!(EncodedOutput, ltr_flags), 74);
    assert_eq!(size_of::<StreamCallbacks>(), 32);
    assert_eq!(offset_of!(StreamCallbacks, release), 24);
}

/// What a native encoder's output thread does: one C call per output.
#[allow(unsafe_code)]
fn hand_over(sink: u64, seq: u64, keyframe: bool, payload: &[u8], config: Option<&[u8]>) -> bool {
    let output = EncodedOutput {
        submit_seq: seq,
        pts_ns: seq * 1_000,
        input_ns: seq * 1_000 + 10,
        output_ns: 0,
        payload: payload.as_ptr(),
        payload_len: payload.len(),
        config: config.map_or(std::ptr::null(), <[u8]>::as_ptr),
        config_len: config.map_or(0, <[u8]>::len),
        flags: if keyframe { ENCODED_KEYFRAME } else { 0 },
        ltr_token: 0,
        temporal_id: -1,
        ltr_flags: 0,
    };
    // SAFETY: `output` and the slices it points to live across the call.
    unsafe { streamcore_video_encoded(sink, &raw const output) }
}

/// The fake encoder's pending frame `seq`, taken as its output thread would.
fn take_pending(encoder: &FakeEncoder, seq: u64) -> SubmitOpts {
    let mut pending = lock(&encoder.pending);
    let index = pending.iter().position(|p| p.seq == seq).expect("pending");
    encoder.log.native_permits.fetch_sub(1, Ordering::SeqCst);
    pending.remove(index).opts
}

fn finish_all(s: &Manual) {
    while s.out.finish_any(0, true) {}
    s.pump();
}

#[test]
fn every_per_frame_call_can_go_through_the_c_abi() {
    let s = Manual::native();
    s.start_running();
    // The first frame gives the shape (UniFFI); later ones repeat it (C).
    s.frame(1_000);
    let encoder = s.encoder();
    let sink = encoder.spec.sink.native_key();
    assert_ne!(sink, 0);
    let opts = take_pending(&encoder, 1);
    assert!(opts.force_keyframe);
    assert!(hand_over(sink, 1, true, b"idr-frame", Some(b"avcC-config")));
    s.pump();
    let packets = s.out.packets();
    assert_eq!(packets.len(), 2);
    assert!(packets[0].meta.config);
    assert_eq!(packets[0].bytes, b"avcC-config");
    assert!(packets[1].meta.keyframe);
    assert_eq!(packets[1].bytes, b"idr-frame");

    // A C output, then a UniFFI one: one wire sequence, one generation.
    s.frame_native(2_000);
    take_pending(&encoder, 2);
    assert!(hand_over(sink, 2, false, b"delta-2", None));
    s.frame_native(3_000);
    encoder.complete(3);
    s.pump();
    let samples = s.out.samples();
    assert_eq!(
        samples.iter().map(|h| h.sequence).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(
        samples
            .iter()
            .all(|h| h.generation == samples[0].generation)
    );
    assert_eq!(s.out.packets()[2].bytes, b"delta-2");
    let submits = lock(&encoder.submits).clone();
    assert_eq!(
        submits.iter().map(|o| o.pts_ns).collect::<Vec<_>>(),
        vec![1_000, 2_000, 3_000]
    );
    finish_all(&s);
    assert_eq!(s.controller.stats().encoded, 3);
    assert_eq!(s.controller.stats().packets_refused, 0);
    s.close();
    s.assert_clean();
    s.assert_wire_order();
}

#[test]
fn the_c_abi_keeps_admission_bounded_and_retains_the_latest_frame() {
    let s = Manual::native();
    s.start_running();
    s.frame(1_000);
    for n in 2..=6 {
        s.frame_native(n * 1_000);
    }
    let encoder = s.encoder();
    // Three in flight; the newest waits in the latest slot (the rest replaced).
    assert_eq!(encoder.pending_seqs(), vec![1, 2, 3]);
    assert_eq!(s.controller.stats().frames_replaced, 2);
    encoder.complete(1);
    s.pump();
    // The sent frame's permit freed a slot: the latest frame goes in.
    assert_eq!(encoder.pending_seqs(), vec![2, 3, 4]);
    assert_eq!(lock(&encoder.submits)[3].pts_ns, 6_000);
    encoder.complete_all();
    s.pump();
    finish_all(&s);
    s.close();
    s.assert_clean();
}

#[test]
fn c_submit_results_map_to_busy_and_failed() {
    let s = Manual::native();
    s.start_running();
    s.frame(1_000);
    let encoder = s.encoder();
    encoder.complete(1);
    s.pump();
    // Busy: retained, resubmitted on input_ready, no sequence consumed.
    encoder.busy_next.store(true, Ordering::SeqCst);
    s.frame_native(2_000);
    assert_eq!(encoder.pending_seqs(), Vec::<u64>::new());
    encoder.ready();
    s.pump();
    assert_eq!(encoder.pending_seqs(), vec![2]);
    encoder.complete(2);
    s.pump();
    // Failed with a recoverable encoder error: recovery, not a stream failure.
    *lock(&encoder.fail_next) = Some(MediaError::EncoderFailed { recoverable: true });
    s.frame_native(3_000);
    s.pump();
    assert!(s.observer.failures().is_empty());
    assert_eq!(s.controller.phase(), StreamPhase::Running);
    s.advance_ms(600);
    s.factory.last().complete_all();
    s.pump();
    finish_all(&s);
    s.close();
    s.assert_clean();
}

#[test]
fn refused_bytes_count_as_a_drop_and_unknown_keys_name_nothing() {
    let s = Manual::native();
    s.start_running();
    s.frame(1_000);
    let encoder = s.encoder();
    let sink = encoder.spec.sink.native_key();
    take_pending(&encoder, 1);
    // An empty payload is refused: the frame is a no-output drop.
    assert!(!hand_over(sink, 1, true, b"", Some(b"avcC")));
    s.pump();
    assert!(s.out.packets().is_empty());
    assert_eq!(s.controller.stats().encoder_drops, 1);
    assert!(!hand_over(u64::MAX, 1, true, b"x", None));
    // SAFETY: NULL is allowed and refused.
    #[allow(unsafe_code)]
    let null = unsafe { streamcore_video_encoded(sink, std::ptr::null()) };
    assert!(!null);
    assert!(!streamcore_video_frame(u64::MAX, CAPTURE, 99, 1, 0));
    streamcore_video_run(u64::MAX);
    s.advance_ms(600);
    s.factory.last().complete_all();
    s.pump();
    finish_all(&s);
    s.close();
    s.assert_clean();
}

#[test]
fn a_closed_streams_keys_name_nothing() {
    let s = Manual::native();
    let port = s.queue.port.native_key();
    s.start_running();
    s.frame(1_000);
    let encoder = s.encoder();
    let sink = encoder.spec.sink.native_key();
    take_pending(&encoder, 1);
    s.close();
    // Closed: no attach, no frame; a late output is not taken.
    assert!(!streamcore_video_attach_stream(
        port,
        StreamCallbacks {
            queue: std::ptr::null_mut(),
            wake: None,
            capture: std::ptr::null_mut(),
            release: None,
        }
    ));
    let late = s.capture.surface(srgb(), 2_000).id;
    assert!(!streamcore_video_frame(port, CAPTURE, late, 2_000, 0));
    CaptureBackend::release(&*s.capture, late);
    assert!(!hand_over(sink, 1, true, b"late", None));
    s.assert_clean();
}

#[test]
fn compact_frames_repeat_the_last_shape() {
    let s = Manual::native();
    s.start_running();
    let port = s.queue.port.native_key();
    // Before any full frame there is no shape: not taken, the caller keeps it.
    let early = s.capture.surface(srgb(), 500).id;
    assert!(!streamcore_video_frame(port, CAPTURE, early, 500, 0));
    s.pump();
    assert!(lock(&s.capture.live).contains(&early));
    assert_eq!(s.controller.stats().frames_received, 0);
    CaptureBackend::release(&*s.capture, early);

    s.frame(1_000);
    let encoder = s.encoder();
    encoder.complete(1);
    s.pump();
    let next = s.capture.surface(srgb(), 2_000).id;
    assert!(streamcore_video_frame(port, CAPTURE, next, 2_000, 1_990));
    s.pump();
    let submits = lock(&encoder.submits).clone();
    assert_eq!(submits.len(), 2);
    // H0 is reported, so it is the image's time.
    assert_eq!(submits[1].capture_ns, 1_990);
    assert_eq!(submits[1].pts_ns, 2_000);
    encoder.complete(2);
    s.pump();

    // Another capture id has no shape: not taken.
    let other = s.capture.surface(srgb(), 3_000).id;
    assert!(!streamcore_video_frame(port, CAPTURE + 1, other, 3_000, 0));
    s.pump();
    CaptureBackend::release(&*s.capture, other);
    assert_eq!(lock(&encoder.submits).len(), 2);

    // A full frame with a new colour changes the shape compact frames repeat:
    // the session is re-created once, not per compact frame.
    s.capture.emit(p3(), 4_000);
    s.pump();
    s.frame_native(5_000);
    assert_eq!(s.factory.count(), 2);
    s.factory.last().complete_all();
    s.pump();
    finish_all(&s);
    s.close();
    s.assert_clean();
}

/// Steady state: a frame is submitted inline, its output posts one wake, and
/// neither the permit released by that drain nor the accepted receipt wakes the
/// queue again. A refusal still reaches the executor.
fn one_wake_per_delivered_frame(s: &Manual, native: bool) {
    s.start_running();
    s.frame(1_000);
    let encoder = s.encoder();
    encoder.complete(1);
    s.pump();
    finish_all(s);
    for n in 2..6 {
        if native {
            s.frame_native(n * 1_000);
        } else {
            s.frame(n * 1_000);
        }
        assert_eq!(s.queue.wakes.load(Ordering::SeqCst), 0);
        encoder.complete(n);
        assert_eq!(s.queue.wakes.swap(0, Ordering::SeqCst), 1);
        s.queue.port.run();
        assert_eq!(s.queue.wakes.load(Ordering::SeqCst), 0);
        assert!(s.out.finish_any(0, true));
        assert_eq!(s.queue.wakes.load(Ordering::SeqCst), 0);
    }
    s.frame(6_000);
    encoder.complete(6);
    s.pump();
    assert!(s.out.finish_any(0, false));
    assert_eq!(s.queue.wakes.load(Ordering::SeqCst), 1);
    s.pump();
    assert_eq!(s.controller.stats().packets_refused, 1);
    // Recovery re-sends the image as a keyframe; finish everything left.
    s.advance_ms(600);
    encoder.complete_all();
    s.pump();
    finish_all(s);
    s.close();
    s.assert_clean();
}

#[test]
fn a_delivered_frame_costs_one_wake() {
    one_wake_per_delivered_frame(&Manual::new(), false);
    one_wake_per_delivered_frame(&Manual::native(), true);
}

/// A restart opens a new capture: the C frame is not taken until a full frame
/// gives the new capture its shape.
#[test]
fn a_restarted_capture_needs_a_full_frame_first() {
    let s = Manual::native();
    s.start_running();
    s.frame(1_000);
    s.frame_native(2_000);
    let encoder = s.encoder();
    encoder.complete_all();
    s.pump();
    finish_all(&s);
    let stop = s.controller.stop();
    s.pump();
    s.capture.answer(Op::Stop, None);
    s.pump();
    assert_eq!(s.observer.results(stop), vec![OpOutcome::Completed]);
    s.start_running();
    let port = s.queue.port.native_key();
    let early = s.capture.surface(srgb(), 3_000).id;
    assert!(!streamcore_video_frame(port, CAPTURE, early, 3_000, 0));
    CaptureBackend::release(&*s.capture, early);
    s.frame(4_000);
    s.frame_native(5_000);
    s.factory.last().complete_all();
    s.pump();
    finish_all(&s);
    s.close();
    s.assert_clean();
}

/// A backend callback that panics inside a drain fails and closes the stream:
/// the capture stops, the close completes and nothing leaks.
fn panicking_submit_closes_the_stream(inline: bool) {
    let s = Manual::new();
    s.start_running();
    s.frame(1_000);
    let encoder = s.encoder();
    encoder.complete(1);
    s.pump();
    while s.out.finish_any(0, true) {}
    s.pump();
    *lock(&encoder.on_submit) = Some(Box::new(|| panic!("backend bug")));
    if inline {
        // The capture callback's inline drain submits.
        s.frame(2_000);
    } else {
        // A busy submit retained the frame; the drain run by `input_ready` submits.
        encoder.busy_next.store(true, Ordering::SeqCst);
        *lock(&encoder.on_submit) = None;
        s.frame(2_000);
        *lock(&encoder.on_submit) = Some(Box::new(|| panic!("backend bug")));
        encoder.ready();
    }
    s.pump();
    assert_eq!(s.observer.failures(), vec![codes::OUTPUT.to_string()]);
    assert_eq!(s.capture.count(Op::Stop), 1, "the capture stops");
    s.capture.answer(Op::Stop, None);
    s.pump();
    assert_eq!(s.controller.close_outcome(), Some(CloseOutcome::Closed));
    assert_eq!(s.observer.closed(), Some(CloseOutcome::Closed));
    assert_eq!(s.controller.phase(), StreamPhase::Closed);
    s.assert_clean();
}

#[test]
fn a_panic_in_an_inline_drain_closes_the_stream() {
    panicking_submit_closes_the_stream(true);
}

#[test]
fn a_panic_in_a_queued_drain_closes_the_stream() {
    panicking_submit_closes_the_stream(false);
}
