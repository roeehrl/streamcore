//! The encoder's failure reports: an `invalidate` that reports `TimedOut`, and
//! a session that fails with no frame in flight (`EncoderEvents::session_failed`).

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use streamcore_video::*;
use support::*;

fn encoder_failures(s: &Manual) -> Vec<MediaError> {
    s.observer
        .jobs()
        .into_iter()
        .filter_map(|job| match job {
            ObserverJob::EncoderFailed { error } => Some(error),
            _ => None,
        })
        .collect()
}

/// `assert_clean` without its peak check: the hung session still held its
/// three frames natively while the new one took a fourth, as intended.
fn assert_hung_session_clean(s: &Manual) {
    assert_eq!(s.fakes.log.native_permits.load(Ordering::SeqCst), 0);
    assert_eq!(s.capture.double_release.load(Ordering::SeqCst), 0);
    assert_eq!(s.capture.leases_outstanding(), 0);
    assert!(s.fakes.log.closed.load(Ordering::SeqCst));
}

/// Fill every permit, then make the next frame need a new session (a colour
/// change), whose teardown invalidates the full one.
fn hang_a_full_session(s: &Manual, mode: InvalidateMode) -> (Arc<FakeEncoder>, Duration) {
    s.start_running();
    let first = s.encoder();
    for n in 1..=3 {
        s.frame(n * 1_000);
    }
    assert_eq!(
        first.pending_seqs(),
        vec![1, 2, 3],
        "every permit is in use"
    );
    *lock(&first.invalidate_mode) = mode;
    s.capture.emit(p3(), 4_000);
    let started = Instant::now();
    s.pump();
    (first, started.elapsed())
}

#[test]
fn an_adapter_bound_below_the_streams_is_still_detected_as_hung() {
    // The shipped adapters bound their own wait at 450 ms, under the stream's
    // 500 ms: the outcome, not the elapsed time, makes the session hung.
    let s = Manual::new();
    let (first, elapsed) =
        hang_a_full_session(&s, InvalidateMode::TimesOut(Duration::from_millis(450)));
    assert!(elapsed >= Duration::from_millis(450), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    assert_eq!(s.controller.stats().invalidate_timeouts, 1);
    assert_eq!(
        encoder_failures(&s),
        vec![MediaError::EncoderFailed { recoverable: true }]
    );
    assert!(s.observer.failures().is_empty(), "the stream continues");

    // The hung session's permits were released: the new session takes the frame.
    let second = s.encoder();
    assert_ne!(second.index, first.index);
    assert_eq!(second.pending_seqs(), vec![4]);
    assert!(lock(&second.submits)[0].force_keyframe);

    // The platform call returns at last: the old session's outputs are stale.
    first.unstick();
    first.complete_all();
    s.pump();
    second.complete_all();
    s.pump();
    assert_eq!(s.out.samples().len(), 1, "only the new session's output");
    s.close();
    assert_hung_session_clean(&s);
}

#[test]
fn a_timed_out_invalidate_is_hung_however_soon_it_returns() {
    let s = Manual::new();
    let (first, elapsed) =
        hang_a_full_session(&s, InvalidateMode::TimesOut(Duration::from_millis(20)));
    assert!(elapsed < Duration::from_millis(400), "{elapsed:?}");
    assert_eq!(s.controller.stats().invalidate_timeouts, 1);
    assert_eq!(encoder_failures(&s).len(), 1);
    assert_eq!(s.encoder().pending_seqs(), vec![4]);
    first.unstick();
    first.complete_all();
    s.encoder().complete_all();
    s.pump();
    s.close();
    assert_hung_session_clean(&s);
}

#[test]
fn a_completed_invalidate_within_the_bound_is_not_hung() {
    let s = Manual::new();
    let (first, _) = hang_a_full_session(&s, InvalidateMode::Quick);
    assert_eq!(s.controller.stats().invalidate_timeouts, 0);
    assert!(encoder_failures(&s).is_empty());
    // Its frames still hold their permits (native code may still use them):
    // the new session waits for them.
    assert!(s.encoder().pending_seqs().is_empty());
    first.complete_all();
    s.pump();
    assert_eq!(s.encoder().pending_seqs(), vec![4]);
    s.encoder().complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn a_recoverable_session_failure_on_a_still_screen_rebuilds_the_encoder() {
    let s = Manual::new();
    s.start_running();
    s.frame(1_000);
    let first = s.encoder();
    first.complete_all();
    s.pump();
    assert_eq!(s.factory.count(), 1);

    // Nothing in flight, no new capture: the session dies on its own.
    first.fail_session(MediaError::EncoderFailed { recoverable: true }, true);
    s.pump();
    assert_eq!(
        s.factory.count(),
        2,
        "rebuilt without waiting for a capture"
    );
    assert!(first.invalidated.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        encoder_failures(&s),
        vec![MediaError::EncoderFailed { recoverable: true }]
    );
    assert!(s.observer.failures().is_empty());

    // No frame was lost: no recovery re-encodes the last image; the new
    // session waits for the next capture.
    s.advance_ms(2_000);
    assert_eq!(s.controller.stats().recoveries, 0);
    assert!(lock(&s.encoder().submits).is_empty());

    // The next capture opens the new session with an IDR.
    s.frame(2_000);
    let second = s.encoder();
    assert_eq!(second.pending_seqs().len(), 1);
    assert!(lock(&second.submits)[0].force_keyframe);
    second.complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn an_unrecoverable_session_failure_fails_the_stream() {
    for (error, recoverable) in [
        (MediaError::EncoderFailed { recoverable: false }, false),
        (MediaError::Internal, false),
        // The error's own flag wins over the call's.
        (MediaError::EncoderFailed { recoverable: false }, true),
    ] {
        let s = Manual::new();
        s.start_running();
        s.frame(1_000);
        s.encoder().complete_all();
        s.pump();
        s.encoder().fail_session(error.clone(), recoverable);
        s.pump();
        assert_eq!(
            s.observer.failures(),
            vec![codes::ENCODE.to_string()],
            "{error:?} {recoverable}"
        );
        assert_eq!(s.factory.count(), 1, "no rebuild");
        assert_eq!(s.capture.count(Op::Stop), 1);
        s.capture.answer(Op::Stop, None);
        s.pump();
        s.close();
        s.assert_clean();
    }
}

#[test]
fn a_session_failure_from_a_replaced_session_is_stale() {
    let s = Manual::new();
    s.start_running();
    s.frame(1_000);
    let first = s.encoder();
    first.complete_all();
    s.pump();
    first.fail_session(MediaError::EncoderFailed { recoverable: true }, true);
    s.pump();
    assert_eq!(s.factory.count(), 2);
    let stale = s.controller.stats().stale_events;
    // The replaced session reports again, even unrecoverably: nothing happens.
    first.fail_session(MediaError::DeviceLost, false);
    s.pump();
    assert_eq!(s.controller.stats().stale_events, stale + 1);
    assert_eq!(s.factory.count(), 2);
    assert!(s.observer.failures().is_empty());
    s.close();
    s.assert_clean();
}

#[test]
fn session_failures_that_repeat_too_often_fail_the_stream() {
    let s = Manual::new();
    s.start_running();
    for n in 0..3 {
        s.encoder()
            .fail_session(MediaError::EncoderFailed { recoverable: true }, true);
        s.pump();
        assert_eq!(s.factory.count(), 2 + n);
    }
    assert!(s.observer.failures().is_empty());
    s.encoder()
        .fail_session(MediaError::EncoderFailed { recoverable: true }, true);
    s.pump();
    assert_eq!(s.observer.failures(), vec![codes::ENCODER.to_string()]);
    s.capture.answer(Op::Stop, None);
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn a_session_failure_with_frames_in_flight_drops_them_with_the_rebuild() {
    let s = Manual::new();
    s.start_running();
    s.frame(1_000);
    let first = s.encoder();
    assert_eq!(first.pending_seqs(), vec![1]);
    first.fail_session(MediaError::EncoderFailed { recoverable: true }, true);
    s.pump();
    assert_eq!(s.factory.count(), 2);
    // The dead session's frame never comes out; the adapter drops it late,
    // stale. It was counted lost with the rebuild: recovery re-encodes the
    // retained image on the new session, with no new capture.
    first.drop_output(1, DropReason::NoOutput);
    s.pump();
    s.advance_ms(600);
    let second = s.encoder();
    assert_eq!(s.controller.stats().recoveries, 1);
    assert_eq!(second.pending_seqs().len(), 1);
    assert!(lock(&second.submits)[0].force_keyframe);
    second.complete_all();
    s.pump();
    assert!(s.observer.failures().is_empty());
    s.close();
    s.assert_clean();
}

#[test]
fn a_frameless_device_loss_rebuilds_like_a_frames_until_it_happens_too_often() {
    // Reported as adapters do (`recoverable: false`): the executor applies the
    // DeviceLost rule, as for a frame dropped with `DeviceLost`.
    let s = Manual::new();
    s.start_running();
    s.frame(1_000);
    s.encoder().complete_all();
    s.pump();
    for n in 0..3 {
        let encoder = s.encoder();
        encoder.fail_session(MediaError::DeviceLost, false);
        s.pump();
        // `recoverable: false` does not fail the stream for DeviceLost.
        assert_eq!(s.factory.count(), 2 + n, "rebuilt, no capture needed");
        assert!(encoder.invalidated.load(Ordering::SeqCst) >= 1);
        assert!(s.observer.failures().is_empty());
        s.advance_ms(1_000);
    }
    // The new session opens with an IDR.
    s.frame(5_000);
    let encoder = s.encoder();
    assert!(lock(&encoder.submits)[0].force_keyframe);
    encoder.complete_all();
    s.pump();
    // A 4th within 10 s fails the stream.
    encoder.fail_session(MediaError::DeviceLost, false);
    s.pump();
    assert_eq!(s.observer.failures(), vec![codes::ENCODER.to_string()]);
    assert_eq!(s.factory.count(), 4);
    s.capture.answer(Op::Stop, None);
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn a_frameless_device_loss_and_a_frames_share_the_rebuild_limit() {
    let s = Manual::new();
    s.start_running();
    for _ in 0..2 {
        s.encoder().fail_session(MediaError::DeviceLost, false);
        s.pump();
    }
    s.frame(1_000);
    let encoder = s.encoder();
    let seq = encoder.pending_seqs()[0];
    encoder.drop_output(
        seq,
        DropReason::Error {
            error: MediaError::DeviceLost,
        },
    );
    s.pump();
    assert_eq!(s.factory.count(), 4);
    assert!(s.observer.failures().is_empty());
    s.encoder().fail_session(MediaError::DeviceLost, true);
    s.pump();
    assert_eq!(s.observer.failures(), vec![codes::ENCODER.to_string()]);
    s.capture.answer(Op::Stop, None);
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn device_losses_older_than_ten_seconds_do_not_count() {
    let s = Manual::new();
    s.start_running();
    for _ in 0..3 {
        s.encoder().fail_session(MediaError::DeviceLost, false);
        s.pump();
    }
    s.advance_ms(10_001);
    s.encoder().fail_session(MediaError::DeviceLost, false);
    s.pump();
    assert_eq!(s.factory.count(), 5);
    assert!(s.observer.failures().is_empty());
    s.close();
    s.assert_clean();
}

#[test]
fn only_failure_driven_rebuilds_count_against_the_budget() {
    let s = Manual::new();
    s.start_running();
    for _ in 0..3 {
        s.encoder()
            .fail_session(MediaError::EncoderFailed { recoverable: true }, true);
        s.pump();
    }
    assert_eq!(s.factory.count(), 4);
    // A rebuild the executor starts itself (a colour change) does not count.
    s.capture.emit(p3(), 1_000);
    s.pump();
    assert_eq!(s.factory.count(), 5);
    // A frame dropped with a recoverable error keeps its session: no rebuild,
    // nothing counted, the loss runs recovery.
    let encoder = s.encoder();
    let seq = encoder.pending_seqs()[0];
    encoder.drop_output(
        seq,
        DropReason::Error {
            error: MediaError::EncoderFailed { recoverable: true },
        },
    );
    s.pump();
    assert_eq!(s.factory.count(), 5);
    assert!(s.observer.failures().is_empty());
    // The 4th failure-driven rebuild within 10 s fails the stream.
    s.encoder().fail_session(MediaError::DeviceLost, false);
    s.pump();
    assert_eq!(s.observer.failures(), vec![codes::ENCODER.to_string()]);
    s.capture.answer(Op::Stop, None);
    s.pump();
    s.close();
    s.assert_clean();
}

/// A failed session as the Windows adapter reports it: its frames dropped
/// (the oldest with the error), then `session_failed` once, all posted before
/// the executor runs.
fn fail_like_windows(s: &Manual, error: MediaError) {
    let encoder = s.encoder();
    let mut reason = Some(DropReason::Error {
        error: error.clone(),
    });
    for seq in encoder.pending_seqs() {
        encoder.drop_output(seq, reason.take().unwrap_or(DropReason::NoOutput));
    }
    let recoverable = error != MediaError::DeviceLost;
    encoder.fail_session(error, recoverable);
}

#[test]
fn a_device_lost_drop_then_session_failed_rebuilds_once_and_counts_once() {
    let s = Manual::new();
    s.start_running();
    for n in 0..3 {
        s.frame(1_000 + n as u64);
        let before = s.controller.stats().stale_events;
        fail_like_windows(&s, MediaError::DeviceLost);
        s.pump();
        assert_eq!(s.factory.count(), 2 + n, "one rebuild per failed session");
        assert_eq!(
            s.controller.stats().stale_events,
            before + 1,
            "the second report is a no-op"
        );
        // Counted once: three failed sessions stay within the budget.
        assert!(s.observer.failures().is_empty(), "session {n}");
    }
    s.frame(2_000);
    fail_like_windows(&s, MediaError::DeviceLost);
    s.pump();
    assert_eq!(s.observer.failures(), vec![codes::ENCODER.to_string()]);
    assert_eq!(s.factory.count(), 4);
    s.capture.answer(Op::Stop, None);
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn recoverable_drops_then_session_failed_rebuild_once_with_one_loss() {
    let s = Manual::new();
    s.start_running();
    s.frame(1_000);
    assert_eq!(s.encoder().pending_seqs(), vec![1]);
    fail_like_windows(&s, MediaError::EncoderFailed { recoverable: true });
    s.pump();
    assert_eq!(s.factory.count(), 2, "one rebuild");
    assert_eq!(
        encoder_failures(&s),
        vec![MediaError::EncoderFailed { recoverable: true }]
    );
    // The drop's loss runs one recovery: the retained image is re-encoded on
    // the new session as a keyframe, without a new capture.
    s.advance_ms(600);
    let second = s.encoder();
    assert_eq!(s.controller.stats().recoveries, 1);
    assert_eq!(second.pending_seqs().len(), 1);
    assert!(lock(&second.submits)[0].force_keyframe);
    second.complete_all();
    s.pump();
    // A frameless failure of every later session on a still screen still
    // fits the budget only three times.
    for _ in 0..2 {
        fail_like_windows(&s, MediaError::EncoderFailed { recoverable: true });
        s.pump();
    }
    assert!(s.observer.failures().is_empty());
    fail_like_windows(&s, MediaError::EncoderFailed { recoverable: true });
    s.pump();
    assert_eq!(s.observer.failures(), vec![codes::ENCODER.to_string()]);
    s.capture.answer(Op::Stop, None);
    s.pump();
    s.close();
    s.assert_clean();
}
