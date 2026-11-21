//! Transport handover (STREAMCORE-VIDEO.md 8.8): a stream parks when its
//! transport goes away, by `detach_transport` or because its channel closed,
//! and continues on another one after `attach_transport` with one keyframe.
//! Frames of the old transport are dropped without counting as lost; a stream
//! parked for longer than `StreamTimings::park_ns` fails.

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use streamcore_video::*;
use support::*;

fn stream() -> Manual {
    let s = Manual::new();
    s.start_running();
    s
}

/// One frame captured, encoded and written.
fn deliver(s: &Manual, out: &FakeOut) {
    s.frame(s.clock.now_ns());
    s.encoder().complete_all();
    s.pump();
    for header in out.held() {
        out.finish(header, true);
    }
    s.pump();
}

fn second_out(s: &Manual) -> (Arc<FakeOut>, Arc<VideoOutput>) {
    let out = FakeOut::new(s.log.clone());
    let output = VideoOutput::with_transport(out.clone(), s.policy(), None);
    (out, output)
}

fn parks(s: &Manual) -> Vec<bool> {
    s.observer
        .jobs()
        .into_iter()
        .filter_map(|job| match job {
            ObserverJob::TransportParked { closed } => Some(closed),
            _ => None,
        })
        .collect()
}

fn assert_no_loss(s: &Manual) {
    let stats = s.controller.stats();
    assert_eq!(stats.recoveries, 0, "a recovery");
    assert_eq!(stats.packets_refused, 0, "a refusal counted");
    assert_eq!(stats.sender_shed, 0, "a shed counted");
    assert_eq!(stats.encoder_drops, 0, "an encoder drop counted");
    assert!(s.observer.failures().is_empty(), "the stream failed");
}

#[test]
fn a_detached_stream_parks_and_continues_on_the_attached_transport_with_one_keyframe() {
    let s = stream();
    deliver(&s, &s.out);
    deliver(&s, &s.out);
    let before = s.out.packets().len();
    let old_generation = s.out.samples().last().unwrap().generation;

    // A frame is in the encoder when the transport goes away.
    s.frame(s.clock.now_ns());
    assert_eq!(s.encoder().pending_seqs().len(), 1);
    s.controller.detach_transport();
    s.pump();
    assert_eq!(parks(&s), vec![false]);
    s.encoder().complete_all();
    s.pump();
    assert_eq!(s.controller.stats().handover_dropped, 1);

    // Parked: frames arrive, nothing is submitted or sent.
    let submitted = s.controller.stats().submitted;
    for _ in 0..3 {
        s.advance_ms(16);
        s.frame(s.clock.now_ns());
    }
    assert_eq!(s.controller.stats().submitted, submitted);
    assert_eq!(s.out.packets().len(), before);

    let (out, output) = second_out(&s);
    let op = s.controller.attach_transport(output, None);
    s.pump();
    assert_eq!(s.observer.results(op), vec![OpOutcome::Completed]);
    // The latest image goes out at once, as a keyframe.
    assert_eq!(s.controller.stats().submitted, submitted + 1);
    assert!(lock(&s.encoder().submits).last().unwrap().force_keyframe);
    s.encoder().complete_all();
    s.pump();
    let packets = out.packets();
    // Every keyframe repeats the configuration.
    assert_eq!(packets.len(), 2, "one keyframe with its configuration");
    assert!(packets[0].meta.config);
    assert!(packets[1].meta.keyframe);
    assert_eq!(
        packets[1].meta.header.generation, old_generation,
        "the same wire generation"
    );
    assert_eq!(s.out.packets().len(), before, "nothing more on the old one");

    // Deltas follow on the new transport.
    for header in out.held() {
        out.finish(header, true);
    }
    deliver(&s, &out);
    assert!(out.packets().last().unwrap().meta.droppable);
    assert!(!lock(&s.encoder().submits).last().unwrap().force_keyframe);
    let stats = s.controller.stats();
    assert_eq!((stats.transport_parks, stats.transport_attaches), (1, 1));
    assert_eq!(s.factory.count(), 1, "the encoder session stayed");
    assert_no_loss(&s);
    s.close();
    s.assert_clean();
}

#[test]
fn a_closed_channel_parks_the_stream_instead_of_failing_it() {
    let s = stream();
    deliver(&s, &s.out);
    s.out.closed.store(true, Ordering::SeqCst);
    s.frame(s.clock.now_ns());
    s.encoder().complete_all();
    s.pump();
    assert_eq!(parks(&s), vec![true]);
    // An explicit detach after it changes nothing.
    s.controller.detach_transport();
    s.pump();
    assert_eq!(parks(&s), vec![true]);
    assert_no_loss(&s);

    let (out, output) = second_out(&s);
    s.controller.attach_transport(output, None);
    s.pump();
    s.encoder().complete_all();
    s.pump();
    assert!(out.packets()[1].meta.keyframe);
    assert_no_loss(&s);
    s.close();
    s.assert_clean();
}

#[test]
fn attaching_while_live_moves_the_stream_without_a_park() {
    let s = stream();
    deliver(&s, &s.out);
    // One frame written but not yet reported, one in the encoder.
    s.frame(s.clock.now_ns());
    s.encoder().complete_all();
    s.pump();
    let unreported = s.out.held();
    assert_eq!(unreported.len(), 1);
    s.frame(s.clock.now_ns());
    let before = s.out.packets().len();

    let (out, output) = second_out(&s);
    let op = s.controller.attach_transport(output, None);
    s.pump();
    assert_eq!(s.observer.results(op), vec![OpOutcome::Completed]);
    assert!(parks(&s).is_empty());
    s.encoder().complete_all();
    s.pump();
    assert_eq!(s.out.packets().len(), before, "nothing more on the old one");
    let keyframes = out.packets().iter().filter(|p| p.meta.keyframe).count();
    assert_eq!(keyframes, 1);
    assert_eq!(s.controller.stats().handover_dropped, 1);

    // The old transport reports its last frame as failed: not this one's loss.
    s.out.finish(unreported[0], false);
    s.pump();
    s.advance_ms(1_000);
    s.encoder().complete_all();
    s.pump();
    assert_no_loss(&s);
    let keyframes = out.packets().iter().filter(|p| p.meta.keyframe).count();
    assert_eq!(keyframes, 1, "no recovery keyframe");
    s.close();
    s.assert_clean();
}

#[test]
fn a_stream_parked_too_long_fails() {
    let s = Manual::with(|_, parts| {
        parts.timings = Some(StreamTimings {
            park_ns: 2 * SECOND,
            ..StreamTimings::default()
        });
    });
    s.start_running();
    deliver(&s, &s.out);
    s.controller.detach_transport();
    s.pump();
    s.advance_ms(1_900);
    assert!(s.observer.failures().is_empty());
    s.advance_ms(200);
    assert_eq!(s.observer.failures(), vec!["stream.transport".to_string()]);
    if s.capture.last_op(Op::Stop).is_some() {
        s.capture.answer(Op::Stop, None);
        s.pump();
    }
    s.close();
    s.assert_clean();
}

#[test]
fn an_attach_in_time_ends_the_park_for_good() {
    let s = stream();
    deliver(&s, &s.out);
    s.controller.detach_transport();
    s.pump();
    s.advance_ms(2_500);
    let (out, output) = second_out(&s);
    s.controller.attach_transport(output, None);
    s.pump();
    s.encoder().complete_all();
    s.pump();
    for header in out.held() {
        out.finish(header, true);
    }
    s.advance_ms(5_000);
    assert!(s.observer.failures().is_empty());
    assert_eq!(s.controller.phase(), StreamPhase::Running);
    deliver(&s, &out);
    assert!(out.packets().last().unwrap().meta.droppable);
    s.close();
    s.assert_clean();
}

#[test]
fn a_second_handover_works_like_the_first() {
    let s = stream();
    deliver(&s, &s.out);
    let (a, output_a) = second_out(&s);
    s.controller.attach_transport(output_a, None);
    s.pump();
    deliver(&s, &a);
    a.closed.store(true, Ordering::SeqCst);
    deliver(&s, &a);
    assert_eq!(parks(&s), vec![true]);
    let (b, output_b) = second_out(&s);
    s.controller.attach_transport(output_b, None);
    s.pump();
    deliver(&s, &b);
    deliver(&s, &b);
    let packets = b.packets();
    assert!(packets[0].meta.config && packets[1].meta.keyframe);
    assert!(packets.last().unwrap().meta.droppable);
    assert_eq!(s.controller.stats().transport_attaches, 2);
    assert_no_loss(&s);
    s.close();
    s.assert_clean();
}

#[test]
fn a_revoked_stream_refuses_an_attach() {
    let s = stream();
    deliver(&s, &s.out);
    s.controller.revoke();
    s.pump();
    let (out, output) = second_out(&s);
    let op = s.controller.attach_transport(output, None);
    s.pump();
    assert_eq!(
        s.observer.results(op),
        vec![OpOutcome::Rejected {
            code: "stream.cancelled".into()
        }]
    );
    s.frame(s.clock.now_ns());
    assert!(out.packets().is_empty());
    s.close();
    s.assert_clean();
}

#[test]
fn an_attach_starts_one_keep_warm_hold() {
    const REFRESH_60: u64 = SECOND / 60;
    let s = Manual::with(|_, parts| {
        parts.plan.refresh_interval_ns = Some(REFRESH_60);
        parts.plan.pacing.keep_warm_ms = 300;
        parts.plan.pacing.rate_adaptation = true;
    });
    s.start_running();
    deliver(&s, &s.out);
    s.controller.detach_transport();
    s.pump();
    let (out, output) = second_out(&s);
    out.sync_receipts.store(true, Ordering::SeqCst);
    s.controller.attach_transport(output, Some(40_000_000));
    s.pump();
    // The user is idle and the screen still: repeats follow the keyframe for
    // the hold, then stop.
    for _ in 0..300 {
        s.queue.advance(2 * MS);
        s.encoder().complete_all();
        s.pump();
    }
    let stats = s.controller.stats();
    assert!(
        (10..=20).contains(&stats.keep_warm_repeats),
        "{} repeats",
        stats.keep_warm_repeats
    );
    let keyframes = out.packets().iter().filter(|p| p.meta.keyframe).count();
    assert_eq!(keyframes, 1);
    assert_no_loss(&s);
    s.close();
    s.assert_clean();
}
