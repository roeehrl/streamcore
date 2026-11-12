//! The reference host's single-queue pipeline tests, ported onto mock traits:
//! revocation, a no-output encoder completion, transport loss under pressure, a
//! client keyframe request, and encoder replacement and stop with stale
//! receipts and codec failures.

mod support;

use std::sync::atomic::Ordering;
use streamcore_video::*;
use support::*;

#[test]
fn encoded_output_cannot_escape_after_ownership_revocation() {
    let s = Manual::new();
    s.start_running();
    s.frame(1_000);
    let encoder = s.encoder();
    assert_eq!(encoder.pending_seqs(), vec![1]);
    // Ownership is revoked while the output is in flight.
    s.controller.revoke();
    encoder.complete(1);
    s.pump();
    s.controller.request_keyframe();
    s.frame(2_000);
    s.advance_ms(550);
    assert!(s.out.packets().is_empty());
    assert_eq!(lock(&encoder.submits).len(), 1);
    s.close();
    s.assert_clean();
}

#[test]
fn encoder_dropped_final_capture_recovers_without_another_offer() {
    // P1's delayed recovery (the 11.1 rollback); P2's immediate one is in
    // tests/pacing.rs.
    let s = Manual::with(|_, parts| parts.plan.pacing = PacingPolicy::p1());
    s.start_running();
    s.frame(1_000);
    let encoder = s.encoder();
    // A no-output completion of the last capture: recovery must not wait for
    // another screen update.
    assert!(encoder.drop_output(1, DropReason::NoOutput));
    s.pump();
    s.advance_ms(499);
    assert!(encoder.pending_seqs().is_empty());
    s.advance_ms(2);
    assert_eq!(encoder.pending_seqs(), vec![2]);
    encoder.complete(2);
    s.pump();
    let samples = s.out.samples();
    assert_eq!(samples.len(), 1);
    assert_eq!((samples[0].sequence, samples[0].flags), (2, 1));
    s.out.finish(samples[0], true);
    s.advance_ms(700);
    assert_eq!(s.out.samples().len(), 1);
    assert!(s.observer.failures().is_empty());
    s.close();
    s.assert_clean();
    s.assert_wire_order();
}

#[test]
fn transport_loss_waits_for_pressure_and_an_older_receipt_cannot_cancel_recovery() {
    let s = Manual::new();
    s.start_running();
    let encoder = s.encoder();
    s.frame(1_000);
    encoder.complete(1);
    s.pump();
    let first = s.out.samples()[0];
    assert_eq!(first.flags, 1, "the first frame of a session is a keyframe");
    s.frame(2_000);
    encoder.complete(2);
    s.pump();
    let lost = s.out.samples()[1];
    s.out.pressured.store(true, Ordering::SeqCst);
    s.out.finish(lost, false);
    // This keyframe was submitted before the lost sample. Its late success must
    // not erase the more recent loss or stop the only recovery timer.
    s.out.finish(first, true);
    s.pump();
    s.advance_ms(700);
    assert_eq!(s.out.samples().len(), 2);
    assert!(encoder.pending_seqs().is_empty());
    s.out.pressured.store(false, Ordering::SeqCst);
    s.advance_ms(600);
    let pending = encoder.pending_seqs();
    assert_eq!(pending.len(), 1);
    encoder.complete(pending[0]);
    s.pump();
    let recovered = s.out.samples()[2];
    assert!(recovered.sequence > lost.sequence && recovered.flags == 1);
    s.out.finish(recovered, true);
    s.advance_ms(700);
    assert_eq!(s.out.samples().len(), 3);
    s.close();
    s.assert_clean();
    s.assert_wire_order();
}

#[test]
fn a_client_request_needs_a_keyframe_submitted_after_the_request() {
    let s = Manual::new();
    s.start_running();
    let encoder = s.encoder();
    s.frame(1_000);
    encoder.complete(1);
    s.pump();
    let old = s.out.samples()[0];
    s.out.pressured.store(true, Ordering::SeqCst);
    s.controller.request_keyframe();
    s.pump();
    s.out.finish(old, true);
    s.pump();
    s.advance_ms(700);
    assert_eq!(s.out.samples().len(), 1);
    s.out.pressured.store(false, Ordering::SeqCst);
    s.advance_ms(600);
    let pending = encoder.pending_seqs();
    assert_eq!(pending.len(), 1);
    encoder.complete(pending[0]);
    s.pump();
    let fresh = s.out.samples()[1];
    assert!(fresh.sequence > old.sequence && fresh.flags == 1);
    s.out.finish(fresh, true);
    s.advance_ms(700);
    assert_eq!(s.out.samples().len(), 2);
    s.close();
    s.assert_clean();
}

#[test]
fn replacement_and_stop_ignore_old_receipts_and_codec_failures() {
    let s = Manual::new();
    s.start_running();
    let first = s.encoder();
    s.frame(1_000);
    first.complete(1);
    s.pump();
    let old = s.out.samples()[0];
    // Replace the encoder (a size change is a new session), then let the old
    // one report a codec failure for its sample.
    let op = s.controller.update(StreamUpdate {
        rate: None,
        capture: CaptureUpdate {
            min_interval_ns: None,
            region: None,
            cursor: None,
        },
        encode_width: Some(32),
        encode_height: Some(32),
    });
    s.pump();
    s.capture.answer(Op::Update, None);
    s.pump();
    assert_eq!(s.observer.results(op), vec![OpOutcome::Completed]);
    assert_eq!(s.factory.count(), 2);
    first.report_drop(
        old.sequence,
        DropReason::Error {
            error: MediaError::EncoderFailed { recoverable: false },
        },
    );
    s.pump();
    let second = s.encoder();
    s.frame(2_000);
    let seq = second.pending_seqs()[0];
    second.complete(seq);
    s.pump();
    let current = s.out.samples()[1];
    assert!(current.generation > old.generation);
    s.out.finish(old, false);
    s.out.finish(current, true);
    s.pump();
    s.advance_ms(700);
    assert_eq!(s.out.samples().len(), 2);
    assert!(s.observer.failures().is_empty());

    // Stop: recovery requested, admission closed, then a late no-output report
    // from the encoder that was current.
    s.controller.request_keyframe();
    s.controller.begin_close();
    s.pump();
    second.report_drop(current.sequence + 1, DropReason::NoOutput);
    s.capture.answer(Op::Stop, None);
    s.pump();
    s.advance_ms(700);
    assert_eq!(s.out.samples().len(), 2);
    assert!(s.observer.failures().is_empty());
    assert_eq!(s.observer.closed(), Some(CloseOutcome::Closed));
    s.assert_clean();
    s.assert_wire_order();
}
