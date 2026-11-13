//! `VideoSink` (STREAMCORE-VIDEO.md 3.4, 6.2, 6.4, 9.4) with its bounds on: R1
//! for an idle sink, the 6.4 shedding rules, `SinkState` and the pacer's gate,
//! the consumed-credit RTT, refusals, revocation, the bulk bucket's rate inputs
//! and `frameTiming`.

mod support;

use std::sync::atomic::Ordering;
use streamcore_media::video_control::FrameTimingReport;
use streamcore_video::*;
use support::*;

fn sink_policy() -> SinkPolicy {
    SinkPolicy {
        timing_every: 0,
        frame_timing_every: 0,
        ..SinkPolicy::default()
    }
}

fn bounded() -> Manual {
    bounded_with(sink_policy(), |_, _| {})
}

fn bounded_with(policy: SinkPolicy, adjust: impl FnOnce(&Fakes, &mut StreamParts)) -> Manual {
    Manual::with(|fakes, parts| {
        parts.output = VideoOutput::with_transport(fakes.out.clone(), policy, None);
        adjust(fakes, parts);
    })
}

fn seqs(s: &Manual) -> Vec<u64> {
    s.out.samples().iter().map(|h| h.sequence).collect()
}

/// Three frames submitted before any output (three permits), so the encoder
/// holds all of them when the sink sees the first.
fn three_in_the_encoder(s: &Manual) -> std::sync::Arc<FakeEncoder> {
    s.start_running();
    s.frame(1_000);
    s.frame(2_000);
    s.frame(3_000);
    let encoder = s.encoder();
    assert_eq!(encoder.pending_seqs(), vec![1, 2, 3]);
    encoder
}

#[test]
fn an_idle_sink_enqueues_inside_push_with_no_hop() {
    let s = bounded();
    s.start_running();
    s.frame(1_000);
    let encoder = s.encoder();
    assert_eq!(s.queue.wakes.load(Ordering::SeqCst), 0);
    encoder.complete(1);
    // The output posts one wake; the drain that handles Encoded enqueues.
    assert_eq!(s.queue.wakes.swap(0, Ordering::SeqCst), 1);
    s.queue.port.run();
    let packets = s.out.packets();
    assert_eq!(packets.len(), 2, "config and keyframe");
    assert!(
        packets.iter().all(|p| p.on_executor),
        "enqueued inside push"
    );
    assert_eq!(
        s.queue.wakes.load(Ordering::SeqCst),
        0,
        "no hop after Encoded"
    );
    let state = s.controller.sink_state();
    assert_eq!(state.unsent_frames, 1);
    // Written and consumed: the sink is idle again.
    s.out.finish_any(0, true);
    s.pump();
    let state = s.controller.sink_state();
    assert_eq!((state.unsent_frames, state.unconsumed_frames), (0, 0));
    s.close();
    s.assert_clean();
    s.assert_wire_order();
}

#[test]
fn one_frame_in_the_transport_and_later_ones_follow_its_write() {
    let s = bounded();
    let encoder = three_in_the_encoder(&s);
    encoder.complete_all();
    s.pump();
    // Only frame 1 is in the transport; 2 and 3 wait in the sink.
    assert_eq!(seqs(&s), vec![1]);
    assert_eq!(s.controller.sink_state().unsent_frames, 3);
    // Admission is held while anything is unsent (6.2).
    s.frame(4_000);
    assert!(encoder.pending_seqs().is_empty());
    assert!(s.controller.stats().sink_held >= 1);
    // The write completion enqueues the next frame itself, off the executor.
    s.out.finish_any(0, true);
    assert_eq!(seqs(&s), vec![1, 2]);
    assert!(!s.out.packets().last().unwrap().on_executor);
    s.out.finish_any(0, true);
    s.out.finish_any(0, true);
    assert_eq!(seqs(&s), vec![1, 2, 3]);
    s.pump();
    // Nothing unsent: the held frame is admitted.
    assert_eq!(encoder.pending_seqs(), vec![4]);
    encoder.complete(4);
    s.pump();
    s.out.finish_any(0, true);
    s.pump();
    assert!(s.controller.stats().sink_unsent_max >= 3);
    s.close();
    s.assert_clean();
    s.assert_wire_order();
}

#[test]
fn rule_1_a_queued_discardable_frame_is_dropped_when_a_newer_one_arrives() {
    let s = bounded();
    let encoder = three_in_the_encoder(&s);
    encoder.complete(1);
    encoder.complete_discardable(2);
    s.pump();
    assert_eq!(s.controller.stats().sink_discardable_shed, 0);
    encoder.complete(3);
    s.pump();
    let stats = s.controller.stats();
    assert_eq!(stats.sink_discardable_shed, 1);
    // A discardable frame breaks no chain: not a loss, no recovery.
    assert_eq!((stats.sender_shed, stats.recoveries), (0, 0));
    while s.out.finish_any(0, true) {}
    s.pump();
    assert_eq!(seqs(&s), vec![1, 3]);
    assert_eq!(s.controller.stats().recoveries, 0);
    s.close();
    s.assert_clean();
}

#[test]
fn rule_1_can_be_switched_off() {
    let s = bounded_with(
        SinkPolicy {
            shed_discardable: false,
            ..sink_policy()
        },
        |_, _| {},
    );
    let encoder = three_in_the_encoder(&s);
    encoder.complete(1);
    encoder.complete_discardable(2);
    encoder.complete(3);
    s.pump();
    while s.out.finish_any(0, true) {}
    s.pump();
    assert_eq!(seqs(&s), vec![1, 2, 3]);
    s.close();
    s.assert_clean();
}

#[test]
fn rule_2_reference_deltas_are_never_shed_by_default() {
    let s = bounded();
    let encoder = three_in_the_encoder(&s);
    encoder.complete(1);
    encoder.complete(2);
    s.pump();
    s.advance_ms(500);
    encoder.complete(3);
    s.pump();
    while s.out.finish_any(0, true) {}
    s.pump();
    assert_eq!(seqs(&s), vec![1, 2, 3]);
    let stats = s.controller.stats();
    assert_eq!((stats.sink_aged_shed, stats.sender_shed), (0, 0));
    assert!(!stats.sink_age_bound);
    s.close();
    s.assert_clean();
}

#[test]
fn rule_2_the_delta_age_bound_needs_confirmed_ltr_or_layers() {
    for confirmed in [false, true] {
        let s = bounded_with(
            SinkPolicy {
                delta_age_bound: true,
                ..sink_policy()
            },
            |_, parts| {
                if confirmed {
                    parts.plan.recovery = vec![RecoveryStep::LtrRefresh, RecoveryStep::Idr];
                }
            },
        );
        let encoder = three_in_the_encoder(&s);
        assert_eq!(s.controller.stats().sink_age_bound, confirmed);
        encoder.complete(1);
        encoder.complete(2);
        s.pump();
        // Past max(100 ms, 3 × RTT) from H5 (no RTT sample yet).
        s.clock.advance(150 * MS);
        encoder.complete(3);
        s.pump();
        let stats = s.controller.stats();
        if confirmed {
            assert_eq!(stats.sink_aged_shed, 1);
            // A shed delta is a loss (skip cause d) and is recovered.
            assert_eq!(stats.sender_shed, 1);
        } else {
            assert_eq!(stats.sink_aged_shed, 0);
        }
        while s.out.finish_any(0, true) {}
        s.pump();
        let sent = seqs(&s);
        if confirmed {
            assert_eq!(&sent[..2], &[1, 3]);
        } else {
            assert_eq!(sent, vec![1, 2, 3]);
        }
        s.advance_ms(600);
        s.encoder().complete_all();
        s.pump();
        while s.out.finish_any(0, true) {}
        s.pump();
        if confirmed {
            assert!(s.controller.stats().recoveries >= 1);
        }
        s.close();
        s.assert_clean();
    }
}

#[test]
fn rule_3_a_keyframe_supersedes_queued_frames_and_config_is_never_shed() {
    let s = bounded();
    s.start_running();
    s.frame(1_000);
    s.frame(2_000);
    // The client asks for a keyframe: the retained image is submitted again
    // as an IDR (sequence 3).
    s.controller.request_keyframe();
    s.pump();
    let encoder = s.encoder();
    assert_eq!(encoder.pending_seqs(), vec![1, 2, 3]);
    encoder.complete_all();
    s.pump();
    let stats = s.controller.stats();
    assert_eq!(stats.sink_superseded, 1, "delta 2 superseded by keyframe 3");
    assert_eq!(stats.sender_shed, 0, "superseded is not a loss");
    // Rule 4: frame 1 was in the transport and stays.
    while s.out.finish_any(0, true) {}
    s.pump();
    assert_eq!(seqs(&s), vec![1, 3]);
    let packets = s.out.packets();
    let configs = packets.iter().filter(|p| p.meta.config).count();
    assert_eq!(configs, 2, "each keyframe keeps its configuration");
    s.close();
    s.assert_clean();
    s.assert_wire_order();
}

#[test]
fn unconsumed_frames_are_bounded_by_the_credit_rtt() {
    let s = bounded();
    s.out.hold_consumed.store(true, Ordering::SeqCst);
    s.start_running();
    let encoder = s.encoder_after(|s| {
        s.frame(1_000);
    });
    let mut next = 2;
    // Two frames written and not consumed: the bound without an RTT sample.
    for seq in 1..=2 {
        if seq > 1 {
            s.frame(seq * 1_000);
        }
        encoder.complete(seq);
        s.pump();
        s.out.finish_any(0, true);
        s.pump();
        next = seq + 1;
    }
    assert_eq!(s.controller.sink_state().unconsumed_frames, 2);
    s.frame(next * 1_000);
    assert!(encoder.pending_seqs().is_empty(), "held at 2 unconsumed");
    // The peer consumes 30 ms after the writes: one RTT sample.
    s.clock.advance(30 * MS);
    s.out.consume_all();
    s.pump();
    let state = s.controller.sink_state();
    assert_eq!(state.credit_rtt_ms, 30);
    assert_eq!(state.unconsumed_frames, 0);
    // `Ready` re-ran admission: the held frame went in.
    assert_eq!(encoder.pending_seqs(), vec![next]);
    assert_eq!(s.controller.stats().credit_rtt_ms, 30);
    // At 60 fps, 30 ms is 2 frames in flight: the bound is now 3.
    for seq in next..next + 3 {
        if seq > next {
            s.frame(seq * 1_000);
        }
        encoder.complete(seq);
        s.pump();
        s.out.finish_any(0, true);
        s.pump();
    }
    assert_eq!(s.controller.sink_state().unconsumed_frames, 3);
    s.frame(10_000);
    assert!(encoder.pending_seqs().is_empty(), "held at 3 unconsumed");
    s.out.consume_all();
    s.pump();
    assert_eq!(encoder.pending_seqs().len(), 1);
    encoder.complete_all();
    s.pump();
    s.out.hold_consumed.store(false, Ordering::SeqCst);
    while s.out.finish_any(0, true) {}
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn an_old_unsent_frame_is_backpressure_and_holds_recovery() {
    let s = bounded();
    s.start_running();
    s.frame(1_000);
    let encoder = s.encoder();
    encoder.complete(1);
    s.pump();
    assert!(!s.controller.sink_state().backpressured);
    s.clock.advance(20 * MS);
    let state = s.controller.sink_state();
    assert_eq!(state.unsent_frames, 1);
    assert!(state.oldest_unsent_age_ms >= 20);
    assert!(state.backpressured, "older than one frame interval");
    s.out.finish_any(0, true);
    s.pump();
    assert!(!s.controller.sink_state().backpressured);
    s.close();
    s.assert_clean();
}

#[test]
fn a_refused_enqueue_is_a_loss_and_recovers() {
    let s = bounded();
    s.start_running();
    s.frame(1_000);
    let encoder = s.encoder();
    encoder.complete(1);
    s.pump();
    s.out.finish_any(0, true);
    s.pump();
    s.out.refuse.store(true, Ordering::SeqCst);
    s.frame(2_000);
    encoder.complete(2);
    s.pump();
    let stats = s.controller.stats();
    assert_eq!((stats.packets_refused, stats.sender_shed), (1, 1));
    assert_eq!(s.controller.sink_state().unsent_frames, 0);
    s.out.refuse.store(false, Ordering::SeqCst);
    // Immediate recovery: the retained image again, as an IDR.
    let pending = encoder.pending_seqs();
    assert_eq!(pending.len(), 1);
    encoder.complete(pending[0]);
    s.pump();
    assert_eq!(s.out.samples().last().unwrap().flags, 1);
    s.out.finish_any(0, true);
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn a_transport_failure_after_enqueue_is_a_loss() {
    let s = bounded();
    s.start_running();
    s.frame(1_000);
    let encoder = s.encoder();
    encoder.complete(1);
    s.pump();
    s.out.finish_any(0, true);
    s.pump();
    s.frame(2_000);
    encoder.complete(2);
    s.pump();
    // The write failed (the ticket dropped unwritten, as on a closed channel).
    s.out.finish_any(0, false);
    s.pump();
    assert_eq!(s.controller.stats().packets_refused, 1);
    assert_eq!(s.controller.sink_state().unsent_frames, 0);
    s.advance_ms(600);
    encoder.complete_all();
    s.pump();
    while s.out.finish_any(0, true) {}
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn revoke_drops_queued_frames_and_nothing_more_is_enqueued() {
    let s = bounded_with(sink_policy(), |fakes, _| {
        fakes.out.bucket.store(true, Ordering::SeqCst);
    });
    let encoder = three_in_the_encoder(&s);
    encoder.complete_all();
    s.pump();
    assert_eq!(seqs(&s), vec![1]);
    s.controller.revoke();
    // Its write completes: nothing queued follows.
    s.out.finish_any(0, true);
    s.pump();
    assert_eq!(seqs(&s), vec![1]);
    assert_eq!(s.controller.sink_state().unsent_frames, 0);
    // The bulk bucket's inputs are removed.
    assert_eq!(lock(&s.out.state).rates.last(), Some(&(0, 0)));
    s.close();
    s.assert_clean();
}

#[test]
fn the_bulk_bucket_gets_rate_inputs_only_when_the_channel_runs_it() {
    // The channel runs the bucket: estimated capacity, at least 2 × video.
    let s = bounded_with(sink_policy(), |fakes, _| {
        fakes.out.bucket.store(true, Ordering::SeqCst);
    });
    s.start_running();
    s.frame(1_000);
    s.encoder().complete(1);
    s.pump();
    let rates = lock(&s.out.state).rates.clone();
    let (capacity, video) = *rates.first().expect("rate inputs");
    assert!(video > 0 && capacity >= 2 * video);
    let stats = s.controller.stats();
    assert!(stats.bulk_capacity_estimated && !stats.bulk_bucket_inactive);
    assert_eq!(stats.video_bps, video);
    s.out.finish_any(0, true);
    s.close();
    s.assert_clean();

    // A known capacity is passed as is.
    let s = Manual::with(|fakes, parts| {
        fakes.out.bucket.store(true, Ordering::SeqCst);
        parts.output =
            VideoOutput::with_transport(fakes.out.clone(), sink_policy(), Some(50_000_000));
    });
    s.start_running();
    s.frame(1_000);
    s.encoder().complete(1);
    s.pump();
    assert_eq!(
        lock(&s.out.state).rates.first().map(|r| r.0),
        Some(50_000_000)
    );
    assert!(!s.controller.stats().bulk_capacity_estimated);
    s.out.finish_any(0, true);
    s.close();
    s.assert_clean();

    // The channel does not run it: reported, and no inputs are set.
    let s = bounded();
    s.start_running();
    s.frame(1_000);
    s.encoder().complete(1);
    s.pump();
    assert!(lock(&s.out.state).rates.is_empty());
    assert!(s.controller.stats().bulk_bucket_inactive);
    s.out.finish_any(0, true);
    s.close();
    s.assert_clean();
}

#[test]
fn frame_timing_is_sent_for_sampled_frames_under_frames_v4_only() {
    for v4 in [false, true] {
        let s = bounded_with(
            SinkPolicy {
                frame_timing_every: 2,
                ..sink_policy()
            },
            |fakes, _| fakes.out.frames_v4.store(v4, Ordering::SeqCst),
        );
        s.start_running();
        let encoder = s.encoder_after(|s| {
            s.frame(1_000);
        });
        for seq in 1..=4 {
            if seq > 1 {
                s.frame(seq * 1_000);
            }
            encoder.complete(seq);
            s.pump();
            s.out.now_us.fetch_add(500, Ordering::SeqCst);
            s.out.finish_any(0, true);
            s.pump();
        }
        let control = lock(&s.out.state).control.clone();
        if !v4 {
            assert!(control.is_empty());
            assert_eq!(s.controller.stats().frame_timing_sent, 0);
        } else {
            assert_eq!(control.len(), 2, "every second written frame");
            let report = FrameTimingReport::decode(&control[0][5..]).expect("frameTiming");
            assert_eq!(control[0][0], 0x14);
            assert_eq!(report.sequence, 2);
            assert_eq!(report.h6_us, report.h5_us + 500);
            // Stats are published by a drain.
            s.controller.request_stats();
            s.pump();
            assert_eq!(s.controller.stats().frame_timing_sent, 2);
        }
        s.close();
        s.assert_clean();
    }
}

#[test]
fn discardable_and_temporal_id_reach_the_wire_under_frames_v4() {
    let s = bounded_with(sink_policy(), |fakes, parts| {
        fakes.out.frames_v4.store(true, Ordering::SeqCst);
        parts.plan.extensions = vec![FrameExtension::Discardable, FrameExtension::TemporalId];
    });
    s.start_running();
    s.frame(1_000);
    let encoder = s.encoder();
    encoder.complete(1);
    s.pump();
    s.out.finish_any(0, true);
    s.pump();
    s.frame(2_000);
    encoder.complete_discardable(2);
    s.pump();
    s.out.finish_any(0, true);
    s.pump();
    let samples: Vec<_> = s
        .out
        .packets()
        .into_iter()
        .filter(|p| !p.meta.config)
        .collect();
    assert!(
        samples[0].meta.extensions.is_empty(),
        "defaults are omitted"
    );
    assert!(samples[1].meta.extensions.discardable);
    assert_eq!(samples[1].meta.extensions.temporal_id, 1);
    s.close();
    s.assert_clean();
}

#[test]
fn the_discard_output_counts_every_frame_written() {
    let s = Manual::with(|_, parts| {
        parts.output = VideoOutput::discard(SinkPolicy::default());
    });
    s.start_running();
    let encoder = s.encoder_after(|s| {
        s.frame(1_000);
    });
    for seq in 1..=5 {
        if seq > 1 {
            s.frame(seq * 1_000);
        }
        encoder.complete(seq);
        s.pump();
        assert_eq!(s.controller.sink_state().unsent_frames, 0);
    }
    assert_eq!(s.controller.stats().encoded, 5);
    s.close();
    s.assert_clean();
}
