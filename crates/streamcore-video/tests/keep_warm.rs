//! Keep-warm (STREAMCORE-VIDEO.md 6.3.1): after input, the latest image is
//! encoded again whenever a refresh interval passes with nothing submitted,
//! for `PacingPolicy::keep_warm_ms`; never while a frame is in the encoder,
//! never while new frames keep coming, and not at all when switched off or
//! without rate adaptation.

mod support;

use streamcore_video::*;
use support::*;

const REFRESH_60: u64 = 1_000_000_000 / 60;

fn stream(keep_warm_ms: u32) -> Manual {
    stream_with(keep_warm_ms, true)
}

fn stream_with(keep_warm_ms: u32, rate_adaptation: bool) -> Manual {
    let s = Manual::with(|_, parts| {
        parts.plan.refresh_interval_ns = Some(REFRESH_60);
        parts.plan.pacing.keep_warm_ms = keep_warm_ms;
        parts.plan.pacing.rate_adaptation = rate_adaptation;
    });
    s.start_running();
    s
}

/// A display frame presented now (H0) and captured 1 ms later (H1).
fn present(s: &Manual) {
    let now = s.clock.now_ns();
    let mut info = s.capture.surface(srgb(), now + MS);
    info.source_present_ns = Some(now);
    s.capture.emit_info(info);
    s.pump();
}

/// Let `ms` pass in 2 ms steps, the encoder finishing everything at once.
fn run_for(s: &Manual, ms: u64) {
    for _ in 0..ms / 2 {
        s.queue.advance(2 * MS);
        s.encoder().complete_all();
        s.pump();
    }
}

#[test]
fn after_input_a_still_screen_is_re_encoded_every_refresh_until_the_hold_ends() {
    let s = stream(500);
    present(&s);
    run_for(&s, 100);
    assert_eq!(
        s.controller.stats().submitted,
        1,
        "no input yet: no repeats"
    );

    s.controller.note_input();
    run_for(&s, 500);
    let stats = s.controller.stats();
    // About one repeat per refresh for 500 ms (each tick lands on a 2 ms step).
    assert!(
        (25..=30).contains(&stats.keep_warm_repeats),
        "{} repeats",
        stats.keep_warm_repeats
    );
    assert_eq!(stats.submitted, 1 + stats.keep_warm_repeats);
    // Repeats are plain deltas of the same image, in order.
    let submits = lock(&s.encoder().submits).clone();
    assert!(submits.iter().skip(1).all(|opts| !opts.force_keyframe));
    assert!(submits.windows(2).all(|w| w[1].pts_ns > w[0].pts_ns));

    // The hold is over: the screen stays still and nothing more is encoded.
    let after = stats.submitted;
    run_for(&s, 300);
    assert_eq!(s.controller.stats().submitted, after);
    s.close();
    s.assert_clean();
}

#[test]
fn more_input_extends_the_hold() {
    let s = stream(200);
    present(&s);
    s.encoder().complete_all();
    for _ in 0..4 {
        s.controller.note_input();
        run_for(&s, 100);
    }
    let repeats = s.controller.stats().keep_warm_repeats;
    // About one per refresh over the 400 ms (each tick lands on a 2 ms step).
    assert!((20..=25).contains(&repeats), "{repeats} repeats");
    run_for(&s, 300);
    assert!(s.controller.stats().keep_warm_repeats - repeats <= 7);
    s.close();
    s.assert_clean();
}

#[test]
fn no_repeat_while_new_frames_keep_coming_or_a_frame_is_in_the_encoder() {
    let s = stream(1_000);
    present(&s);
    s.encoder().complete_all();
    s.controller.note_input();
    // A frame on every refresh: the screen is changing, nothing to repeat.
    for _ in 0..30 {
        s.queue.advance(REFRESH_60);
        present(&s);
        s.encoder().complete_all();
        s.pump();
    }
    assert_eq!(s.controller.stats().keep_warm_repeats, 0);

    // The encoder holds the last frame: a repeat would only queue behind it.
    s.queue.advance(REFRESH_60);
    present(&s);
    s.queue.advance(100 * MS);
    assert_eq!(s.controller.stats().keep_warm_repeats, 0);
    assert_eq!(s.encoder().pending_seqs().len(), 1);
    s.encoder().complete_all();
    s.pump();
    run_for(&s, 50);
    assert!(s.controller.stats().keep_warm_repeats >= 2);
    s.close();
    s.assert_clean();
}

#[test]
fn switched_off_input_changes_nothing() {
    let s = stream(0);
    present(&s);
    s.controller.note_input();
    run_for(&s, 300);
    let stats = s.controller.stats();
    assert_eq!((stats.submitted, stats.keep_warm_repeats), (1, 0));
    s.close();
    s.assert_clean();
}

#[test]
fn without_rate_adaptation_input_changes_nothing() {
    let s = stream_with(1_000, false);
    present(&s);
    s.controller.note_input();
    run_for(&s, 300);
    let stats = s.controller.stats();
    assert_eq!((stats.submitted, stats.keep_warm_repeats), (1, 0));
    s.close();
    s.assert_clean();
}

#[test]
fn a_screen_changing_at_half_the_refresh_rate_gets_no_repeats_between_frames() {
    let s = stream(1_000);
    present(&s);
    s.encoder().complete_all();
    // 30 fps on a 60 Hz display, a new image every two refresh intervals, input all along.
    s.controller.note_input();
    for _ in 0..30 {
        s.controller.note_input();
        for _ in 0..2 {
            s.queue.advance(REFRESH_60);
            s.encoder().complete_all();
            s.pump();
        }
        present(&s);
        s.encoder().complete_all();
        s.pump();
    }
    // A few may come while the rate is still being learnt, none after.
    let early = s.controller.stats().keep_warm_repeats;
    assert!(early <= 3, "{early} repeats while learning");
    for _ in 0..30 {
        s.controller.note_input();
        for _ in 0..2 {
            s.queue.advance(REFRESH_60);
            s.encoder().complete_all();
            s.pump();
        }
        present(&s);
        s.encoder().complete_all();
        s.pump();
    }
    assert_eq!(s.controller.stats().keep_warm_repeats, early);
    // The screen stops while the pen is still active: repeats start again.
    s.controller.note_input();
    run_for(&s, 200);
    assert!(s.controller.stats().keep_warm_repeats > early + 5);
    s.close();
    s.assert_clean();
}
