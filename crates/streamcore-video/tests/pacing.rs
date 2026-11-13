//! P2 pacing (STREAMCORE-VIDEO.md 6.2, 6.3, 6.5, 6.7): refresh-aligned
//! admission, adaptive encoder permits, drop before encode, immediate first
//! recovery and the skip counters, each behind its `PacingPolicy` switch.

mod support;

use streamcore_video::pacing::{MAX_PERMITS, PERMIT_MIN_SAMPLES, PermitGovernor, RefreshPacer};
use streamcore_video::*;
use support::*;

const NS_PER_SECOND: u64 = 1_000_000_000;

fn interval(hz: u64) -> u64 {
    NS_PER_SECOND / hz
}

/// Frames presented on every refresh of a `panel_hz` panel for one second,
/// offered to a pacer targeting `fps`; returns the admitted H0s.
fn admitted_over_one_second(panel_hz: u64, fps: u64, refresh: Option<u64>) -> Vec<u64> {
    let mut pacer = RefreshPacer::new(interval(fps), refresh.unwrap_or(0));
    (0..panel_hz)
        .map(|k| k * NS_PER_SECOND / panel_hz)
        .filter(|&h0| pacer.offer(h0))
        .collect()
}

#[test]
fn a_144_hz_panel_gives_a_60_fps_stream_60_frames_not_72() {
    let refresh = interval(144);
    let admitted = admitted_over_one_second(144, 60, Some(refresh));
    assert_eq!(admitted.len(), 60);
    // Each within half a refresh of its due time, which advances by the target
    // interval from the previous due time.
    for (n, h0) in admitted.iter().enumerate() {
        let due = n as u64 * interval(60);
        assert!(
            h0 + refresh / 2 >= due && *h0 < due + refresh,
            "frame {n} at {h0} for due {due}"
        );
    }
    // The rule 6.3 rejects: measuring from the last admitted H0 admits every
    // second refresh (13.89 ms >= 16.667 - 3.472 ms), 72 frames per second.
    let mut last: Option<u64> = None;
    let naive = (0..144)
        .map(|k| k * NS_PER_SECOND / 144)
        .filter(|&h0| {
            let ok = last.is_none_or(|l| h0 + refresh / 2 >= l + interval(60));
            if ok {
                last = Some(h0);
            }
            ok
        })
        .count();
    assert_eq!(naive, 72);
}

#[test]
fn sixty_and_120_hz_panels_pace_to_the_target() {
    assert_eq!(
        admitted_over_one_second(60, 60, Some(interval(60))).len(),
        60
    );
    assert_eq!(
        admitted_over_one_second(120, 60, Some(interval(120))).len(),
        60
    );
    assert_eq!(
        admitted_over_one_second(120, 120, Some(interval(120))).len(),
        120
    );
    assert_eq!(
        admitted_over_one_second(120, 30, Some(interval(120))).len(),
        30
    );
    assert_eq!(
        admitted_over_one_second(144, 30, Some(interval(144))).len(),
        30
    );
    // A 60 Hz panel whose present times jitter by up to ±1 ms keeps 60.
    let mut pacer = RefreshPacer::new(interval(60), interval(60));
    let jitter = [0i64, 900_000, -900_000, 400_000, -1_000_000, 1_000_000];
    let count = (0..60u64)
        .map(|k| (k * interval(60)) as i64 + jitter[k as usize % jitter.len()])
        .filter(|&h0| pacer.offer(h0.max(0) as u64))
        .count();
    assert_eq!(count, 60);
}

#[test]
fn the_due_time_resynchronises_after_idle() {
    let refresh = interval(144);
    let mut pacer = RefreshPacer::new(interval(60), refresh);
    for k in 0..72 {
        pacer.offer(k * refresh);
    }
    let idle_until = 72 * refresh + 2 * NS_PER_SECOND;
    // The first frame after the idle gap is admitted at once and the cadence
    // restarts from it: no burst to catch up the missed due times.
    assert!(pacer.offer(idle_until));
    assert_eq!(pacer.due_ns(), Some(idle_until + interval(60)));
    let after = (1..144)
        .map(|k| idle_until + k * refresh)
        .filter(|&h0| h0 < idle_until + NS_PER_SECOND && pacer.offer(h0))
        .count();
    assert_eq!(after + 1, 60);
}

#[test]
fn the_hold_deadline_is_absolute_on_the_host_clock() {
    // A 60 Hz Windows source: H0 (the display time) comes 12 ms after H1.
    let lead = 12 * MS;
    let mut pacer = RefreshPacer::new(interval(60), interval(60));
    assert!(pacer.offer(lead));
    let due = lead + interval(60);
    assert_eq!(pacer.due_ns(), Some(due));
    // An early frame arrives at H1 = 8 ms with H0 = 20 ms: held.
    let (h1, h0) = (8 * MS, 8 * MS + lead);
    assert!(!pacer.offer(h0));
    let deadline = pacer.hold_deadline_ns(h0);
    assert_eq!(deadline, due + interval(60) / 2 + MS);
    // Armed from the host's now (H1), not from H0: it fires at the deadline,
    // not one H1 -> H0 early.
    assert_eq!(h1 + pacer.hold_delay_ns(h0, h1), deadline);
    // Capped as before, at a target interval plus a refresh from now; zero
    // once the deadline has passed.
    assert_eq!(pacer.hold_delay_ns(h0, 0), interval(60) * 2);
    assert_eq!(pacer.hold_delay_ns(h0, deadline + MS), 0);
    // The held frame admitted at the deadline keeps the cadence's phase: the
    // next frame on time is admitted.
    pacer.admit_held(deadline);
    assert_eq!(pacer.due_ns(), Some(due + interval(60)));
    assert!(pacer.offer(due + interval(60)));
}

#[test]
fn h0_after_capture_still_paces_and_resyncs_after_idle() {
    // H0 = H1 + 12 ms on a 60 Hz panel with ±1 ms jitter: 60 of 60, and every
    // admission within half a refresh of its due time.
    let lead = 12 * MS;
    let mut pacer = RefreshPacer::new(interval(60), interval(60));
    let admitted = (0..60u64)
        .map(|k| k * interval(60) + lead + if k % 2 == 0 { MS } else { 0 })
        .filter(|&h0| pacer.offer(h0))
        .count();
    assert_eq!(admitted, 60);
    // Idle for a second, then frames again: resynchronised, 60 per second.
    let idle_until = 2 * NS_PER_SECOND + lead;
    assert!(pacer.offer(idle_until));
    assert_eq!(pacer.due_ns(), Some(idle_until + interval(60)));
    let after = (1..60u64)
        .map(|k| idle_until + k * interval(60))
        .filter(|&h0| pacer.offer(h0))
        .count();
    assert_eq!(after + 1, 60);
}

#[test]
fn without_a_refresh_interval_the_pacer_falls_back_to_the_target() {
    let pacer = RefreshPacer::new(interval(60), 0);
    assert_eq!(pacer.refresh_ns(), interval(60));
    // A 60 Hz panel still gives 60.
    assert_eq!(admitted_over_one_second(60, 60, None).len(), 60);
}

#[test]
fn the_permit_target_adds_one_for_slow_encodes_and_caps_at_three() {
    let frame = interval(60);
    let mut permits = PermitGovernor::new(1);
    assert_eq!(permits.target(), 1);
    // Fast encodes: the base.
    for _ in 0..PERMIT_MIN_SAMPLES * 2 {
        permits.record(frame / 4, frame);
    }
    assert_eq!(permits.target(), 1);
    // p95 above 0.8 x interval: one more permit.
    for _ in 0..PERMIT_MIN_SAMPLES * 4 {
        permits.record(frame * 9 / 10, frame);
    }
    assert_eq!(permits.target(), 2);
    // Back below 0.6 x interval: the base again.
    for _ in 0..PERMIT_MIN_SAMPLES * 4 {
        permits.record(frame / 2, frame);
    }
    assert_eq!(permits.target(), 1);
    // Never above the hard cap.
    let mut permits = PermitGovernor::new(3);
    for _ in 0..PERMIT_MIN_SAMPLES * 4 {
        permits.record(frame * 2, frame);
    }
    assert_eq!(permits.target(), MAX_PERMITS);
}

// ---------------------------------------------------------------------------
// Through the executor.

/// A display frame presented now (H0) and captured 1 ms later (H1).
fn present(s: &Manual) -> SurfaceId {
    let now = s.clock.now_ns();
    let mut info = s.capture.surface(srgb(), now + MS);
    info.source_present_ns = Some(now);
    let id = info.id;
    s.capture.emit_info(info);
    s.pump();
    id
}

fn paced(refresh: Option<u64>, adjust: impl FnOnce(&mut StreamPlan)) -> Manual {
    let s = Manual::with(|_, parts| {
        parts.plan.refresh_interval_ns = refresh;
        adjust(&mut parts.plan);
    });
    s.start_running();
    s
}

#[test]
fn the_executor_submits_60_of_144_frames_per_second() {
    let s = paced(Some(interval(144)), |_| {});
    for _ in 0..144 {
        present(&s);
        s.encoder().complete_all();
        s.queue.advance(interval(144));
    }
    let stats = s.controller.stats();
    assert_eq!(stats.submitted, 60);
    // The other 84 were early: each but the last was replaced while held (not
    // a skip), and no hold timer fired ahead of a due frame.
    assert_eq!(stats.pacer_held, 83);
    assert_eq!(stats.pacer_timer_admits, 0);
    assert_eq!(stats.frames_replaced, 0, "nothing waited for a permit");
    assert_eq!(stats.refresh_interval_ns, interval(144));
    assert!(!stats.refresh_fallback);
    s.close();
    s.assert_clean();
}

#[test]
fn a_held_frame_on_a_still_screen_is_submitted_when_due() {
    let s = paced(Some(interval(144)), |_| {});
    present(&s);
    let encoder = s.encoder();
    assert_eq!(encoder.pending_seqs(), vec![1]);
    encoder.complete_all();
    s.queue.advance(interval(144));
    // Early: held, not submitted.
    present(&s);
    assert!(encoder.pending_seqs().is_empty());
    // The screen goes still: the held frame is the last change and is sent
    // within one target interval plus a refresh.
    s.queue.advance(interval(60));
    assert_eq!(encoder.pending_seqs(), vec![2]);
    let stats = s.controller.stats();
    assert_eq!(stats.pacer_timer_admits, 1);
    assert_eq!(stats.submitted, 2);
    encoder.complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn early_frames_replace_the_held_one_and_are_not_skips() {
    let s = paced(Some(interval(240)), |_| {});
    present(&s);
    s.encoder().complete_all();
    for _ in 0..3 {
        s.queue.advance(interval(240));
        present(&s);
    }
    let stats = s.controller.stats();
    assert_eq!(stats.submitted, 1);
    assert_eq!(stats.pacer_held, 2);
    assert_eq!(stats.frames_replaced, 0);
    s.close();
    s.assert_clean();
}

#[test]
fn a_due_frame_without_a_permit_is_replaced_by_the_newer_one_skip_b() {
    // One permit, pacing off: a frame that cannot get a permit is dropped
    // before encode by the next one (D7), never queued.
    let s = paced(None, |plan| {
        plan.permit_target = 1;
        plan.pacing.refresh_aligned = false;
    });
    present(&s);
    let encoder = s.encoder();
    assert_eq!(encoder.pending_seqs(), vec![1]);
    for _ in 0..3 {
        s.queue.advance(interval(60));
        present(&s);
    }
    assert_eq!(encoder.pending_seqs(), vec![1]);
    assert_eq!(s.controller.stats().frames_replaced, 2);
    // The permit frees: only the newest frame is encoded.
    encoder.complete_all();
    s.pump();
    assert_eq!(encoder.pending_seqs(), vec![2]);
    assert_eq!(s.controller.stats().submitted, 2);
    encoder.complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn a_due_frame_waiting_for_a_permit_keeps_its_admission_when_replaced() {
    let s = paced(Some(interval(60)), |plan| plan.permit_target = 1);
    present(&s);
    let encoder = s.encoder();
    s.queue.advance(interval(60));
    // Due, but the only permit is busy: waits in the latest slot.
    present(&s);
    s.queue.advance(MS);
    // A newer frame replaces it before the permit frees: skip cause (b), and
    // the newer frame inherits the admission.
    present(&s);
    assert_eq!(s.controller.stats().frames_replaced, 1);
    encoder.complete_all();
    s.pump();
    assert_eq!(encoder.pending_seqs(), vec![2]);
    encoder.complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn the_refresh_interval_follows_display_mode_changes() {
    let s = paced(None, |_| {});
    let stats = s.controller.stats();
    assert!(
        stats.refresh_fallback,
        "no refresh interval: target interval"
    );
    assert_eq!(stats.refresh_interval_ns, interval(60));
    s.controller.set_refresh_interval(Some(interval(120)));
    s.pump();
    let stats = s.controller.stats();
    assert_eq!(stats.refresh_interval_ns, interval(120));
    assert!(!stats.refresh_fallback);
    // Nonsense is unknown.
    s.controller.set_refresh_interval(Some(5));
    s.pump();
    assert!(s.controller.stats().refresh_fallback);
    s.close();
}

#[test]
fn frames_without_a_present_time_are_admitted_unpaced() {
    let s = paced(Some(interval(144)), |_| {});
    for n in 0..4 {
        s.frame(1_000 + n);
        s.encoder().complete_all();
        s.pump();
    }
    assert_eq!(s.controller.stats().submitted, 4);
    s.close();
    s.assert_clean();
}

#[test]
fn refresh_alignment_off_admits_every_frame() {
    let s = paced(Some(interval(144)), |plan| {
        plan.pacing.refresh_aligned = false
    });
    for _ in 0..12 {
        present(&s);
        s.encoder().complete_all();
        s.queue.advance(interval(144));
    }
    let stats = s.controller.stats();
    assert_eq!(stats.submitted, 12);
    assert_eq!(stats.refresh_interval_ns, 0);
    s.close();
    s.assert_clean();
}

#[test]
fn slow_encodes_raise_the_permit_target_by_one() {
    let s = paced(Some(interval(60)), |plan| {
        plan.permit_target = 1;
        plan.pacing.refresh_aligned = false;
    });
    assert_eq!(s.controller.stats().permit_target, 1);
    // Each encode takes 15 ms of a 16.7 ms interval (> 0.8 x).
    for _ in 0..PERMIT_MIN_SAMPLES + 2 {
        present(&s);
        s.queue.advance(15 * MS);
        s.encoder().complete_all();
        s.pump();
        s.queue.advance(2 * MS);
    }
    let stats = s.controller.stats();
    assert_eq!(stats.permit_target, 2);
    assert!(stats.encode_p95_us >= 15_000);
    // Two frames can now be in the encoder.
    present(&s);
    s.queue.advance(5 * MS);
    present(&s);
    assert_eq!(s.encoder().pending_seqs().len(), 2);
    s.encoder().complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn adaptive_permits_off_keeps_the_plan_target() {
    let s = paced(Some(interval(60)), |plan| {
        plan.permit_target = 1;
        plan.pacing.refresh_aligned = false;
        plan.pacing.adaptive_permits = false;
    });
    for _ in 0..PERMIT_MIN_SAMPLES + 2 {
        present(&s);
        s.queue.advance(15 * MS);
        s.encoder().complete_all();
        s.pump();
        s.queue.advance(2 * MS);
    }
    assert_eq!(s.controller.stats().permit_target, 1);
    s.close();
    s.assert_clean();
}

#[test]
fn host_detected_loss_recovers_at_once_then_at_most_twice_a_second() {
    let s = paced(None, |plan| plan.pacing.refresh_aligned = false);
    let encoder = s.encoder();
    s.frame(1_000);
    encoder.complete_all();
    s.pump();
    s.out.finish(s.out.samples()[0], true);
    s.pump();
    // A no-output completion of the last capture: recovered on the next wake,
    // not after 0.5 s.
    s.frame(2_000);
    assert!(encoder.drop_output(2, DropReason::NoOutput));
    s.pump();
    assert_eq!(encoder.pending_seqs(), vec![3]);
    assert!(encoder_submit(&s, 3).force_keyframe);
    assert_eq!(s.controller.stats().recoveries, 1);
    // That IDR is lost too: the next recovery waits for the interval.
    assert!(encoder.drop_output(3, DropReason::NoOutput));
    s.pump();
    s.queue.advance(400 * MS);
    assert!(encoder.pending_seqs().is_empty());
    s.queue.advance(110 * MS);
    assert_eq!(encoder.pending_seqs(), vec![4]);
    assert_eq!(s.controller.stats().recoveries, 2);
    encoder.complete_all();
    s.pump();
    let last = *s.out.samples().last().expect("the recovery keyframe");
    assert_eq!(last.flags, 1);
    s.out.finish(last, true);
    s.queue.advance(1_000 * MS);
    assert_eq!(
        s.controller.stats().recoveries,
        2,
        "recovered: no more IDRs"
    );
    s.close();
    s.assert_clean();
}

#[test]
fn immediate_recovery_off_waits_the_recovery_delay() {
    let s = paced(None, |plan| {
        plan.pacing.refresh_aligned = false;
        plan.pacing.immediate_recovery = false;
    });
    let encoder = s.encoder();
    s.frame(1_000);
    assert!(encoder.drop_output(1, DropReason::NoOutput));
    s.pump();
    s.queue.advance(490 * MS);
    assert!(encoder.pending_seqs().is_empty());
    s.queue.advance(20 * MS);
    assert_eq!(encoder.pending_seqs(), vec![2]);
    encoder.complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn sender_shed_and_client_flush_are_counted() {
    let s = paced(None, |plan| plan.pacing.refresh_aligned = false);
    let encoder = s.encoder();
    s.frame(1_000);
    encoder.complete_all();
    s.pump();
    s.out.finish(s.out.samples()[0], true);
    s.frame(2_000);
    encoder.complete_all();
    s.pump();
    // The sender sheds a delta: skip cause (d).
    s.out.finish(s.out.samples()[1], false);
    s.controller.note_client_flush(3);
    s.pump();
    let stats = s.controller.stats();
    assert_eq!(stats.sender_shed, 1);
    assert_eq!(stats.packets_refused, 1);
    assert_eq!(stats.client_flushed, 3);
    encoder.complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

fn encoder_submit(s: &Manual, seq: u64) -> SubmitOpts {
    *lock(&s.encoder().submits)
        .iter()
        .find(|opts| opts.submit_seq == seq)
        .expect("submitted")
}

/// A Windows-style frame: captured now (H1) and presented `lead` later (H0,
/// the composition's display time).
fn present_ahead(s: &Manual, lead: u64) -> SurfaceId {
    let now = s.clock.now_ns();
    let mut info = s.capture.surface(srgb(), now);
    info.source_present_ns = Some(now + lead);
    let id = info.id;
    s.capture.emit_info(info);
    s.pump();
    id
}

#[test]
fn h0_twelve_ms_after_h1_still_submits_60_frames_per_second_at_60_hz() {
    let s = paced(Some(interval(60)), |_| {});
    for k in 0..60u64 {
        // The measured H1 -> H0 spread: about +12 ms, a little jitter.
        present_ahead(&s, 12 * MS + (k % 3) * MS / 2);
        s.encoder().complete_all();
        s.queue.advance(interval(60));
    }
    let stats = s.controller.stats();
    assert_eq!(stats.submitted, 60);
    assert_eq!(stats.pacer_held, 0);
    assert_eq!(stats.pacer_timer_admits, 0);
    s.close();
    s.assert_clean();
}

#[test]
fn a_held_frame_fires_on_the_host_clock_when_h0_comes_after_capture() {
    let lead = 12 * MS;
    let s = paced(Some(interval(60)), |_| {});
    let t0 = s.clock.now_ns();
    present_ahead(&s, lead);
    let encoder = s.encoder();
    assert_eq!(encoder.pending_seqs(), vec![1]);
    encoder.complete_all();
    s.pump();
    // Due at H0 = t0 + 12 ms + one interval. A frame arriving 8 ms in (H0 =
    // t0 + 20 ms) is early: held.
    let due = t0 + lead + interval(60);
    s.queue.advance(8 * MS);
    present_ahead(&s, lead);
    assert!(encoder.pending_seqs().is_empty());
    // Its timer fires at due + refresh / 2 + 1 ms on the host clock. A delay
    // measured from H0 would fire 12 ms earlier (t0 + 26 ms).
    let deadline = due + interval(60) / 2 + MS;
    s.queue.advance(deadline - MS - s.clock.now_ns());
    assert!(
        encoder.pending_seqs().is_empty(),
        "fired early, at {}",
        s.clock.now_ns() - t0
    );
    s.queue.advance(MS);
    assert_eq!(encoder.pending_seqs(), vec![2]);
    assert_eq!(s.controller.stats().pacer_timer_admits, 1);
    encoder.complete_all();
    s.pump();
    // The cadence keeps its phase: due one interval after the held frame's
    // slot, so the next frame (H1 = t0 + 45 ms, H0 = t0 + 57 ms) is admitted.
    s.queue.advance(t0 + 45 * MS - s.clock.now_ns());
    present_ahead(&s, lead);
    assert_eq!(encoder.pending_seqs(), vec![3]);
    encoder.complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}
