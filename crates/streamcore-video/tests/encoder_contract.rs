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
fn a_device_lost_or_unrecoverable_session_failure_fails_the_stream() {
    for (error, recoverable) in [
        (MediaError::DeviceLost, false),
        (MediaError::EncoderFailed { recoverable: false }, false),
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
    // The dead session's frame never comes out; the adapter drops it late.
    first.drop_output(1, DropReason::NoOutput);
    s.pump();
    s.frame(2_000);
    let second = s.encoder();
    assert_eq!(second.pending_seqs().len(), 1);
    second.complete_all();
    s.pump();
    assert!(s.observer.failures().is_empty());
    s.close();
    s.assert_clean();
}
