//! The recovery ladder (STREAMCORE-VIDEO.md 6.5) with LTR and temporal layers:
//! an LTR refresh only with confirmed caps and a token the client acknowledged
//! after decode on the current transport; an IDR otherwise, and as the
//! escalation when a refresh fails; later IDRs at most twice a second; LTR
//! state cleared on generation and transport changes; discardable frames of a
//! two-layer encoder shed without recovery (6.4 rule 1); the delta age bound
//! only with confirmed caps (rule 2).

mod support;

use streamcore_video::*;
use support::*;

const LTR: [RecoveryStep; 2] = [RecoveryStep::LtrRefresh, RecoveryStep::Idr];

/// Caps as a probe that confirmed LTR and two temporal layers reports.
fn confirmed_caps() -> EncoderCaps {
    EncoderCaps {
        codec: Codec::H264,
        name: "fake".into(),
        hardware: true,
        ltr: FeatureSupport::Confirmed,
        temporal_layers: FeatureSupport::Confirmed,
        intra_refresh: FeatureSupport::Unsupported,
        low_latency_rate_control: FeatureSupport::Confirmed,
        max_width: 4096,
        max_height: 4096,
        max_fps: 120,
        inputs: vec![PixelFormat::Nv12],
        force_idr: true,
        dynamic_rate: true,
        min_inputs_before_output: 1,
        vui_checked: true,
        max_frame_qp: false,
    }
}

/// A running stream with unpaced admission and `recovery` in its plan.
fn ladder(recovery: &[RecoveryStep], adjust: impl FnOnce(&Fakes, &mut StreamParts)) -> Manual {
    let s = Manual::with(|fakes, parts| {
        parts.plan.pacing.refresh_aligned = false;
        parts.plan.recovery = recovery.to_vec();
        if recovery.contains(&RecoveryStep::LtrRefresh) {
            parts.plan.extensions = vec![FrameExtension::Ltr];
        }
        adjust(fakes, parts);
    });
    s.start_running();
    s
}

fn submit(s: &Manual, seq: u64) -> SubmitOpts {
    *lock(&s.encoder().submits)
        .iter()
        .find(|opts| opts.submit_seq == seq)
        .expect("submitted")
}

/// Encode, send and write one frame at `capture_ns`; returns its sequence.
fn deliver(s: &Manual, capture_ns: u64) -> u64 {
    s.frame(capture_ns);
    let encoder = s.encoder();
    let seq = *encoder.pending_seqs().last().expect("submitted");
    assert!(encoder.complete(seq));
    s.pump();
    let header = *s.out.samples().last().expect("sent");
    assert_eq!(header.sequence, seq);
    s.out.finish(header, true);
    s.pump();
    seq
}

fn generation(s: &Manual) -> u64 {
    s.out.samples().last().expect("sent").generation
}

/// Two frames out (an IDR marking token 1, a delta marking token 2) and the
/// client's ack of token 2, which the next submit hands to the encoder.
fn acked(s: &Manual) {
    deliver(s, 1_000);
    deliver(s, 2_000);
    s.controller.note_ltr_ack(generation(s), 2);
    s.pump();
    assert_eq!(s.controller.stats().ltr_acks, 1);
    let seq = deliver(s, 3_000);
    assert_eq!(submit(s, seq).ltr_acknowledged, Some(2));
}

/// A no-output completion of the next frame: a host-detected loss.
fn lose_next(s: &Manual, capture_ns: u64) -> u64 {
    s.frame(capture_ns);
    let encoder = s.encoder();
    let seq = *encoder.pending_seqs().last().expect("submitted");
    assert!(encoder.drop_output(seq, DropReason::NoOutput));
    s.pump();
    seq
}

#[test]
fn an_acknowledged_token_makes_the_first_recovery_an_ltr_refresh() {
    let s = ladder(&LTR, |_, _| {});
    acked(&s);
    let encoder = s.encoder();
    let lost = lose_next(&s, 4_000);
    // At once, as a delta predicted from the acknowledged frame.
    let refresh = lost + 1;
    assert_eq!(encoder.pending_seqs(), vec![refresh]);
    let opts = submit(&s, refresh);
    assert!(opts.ltr_refresh && !opts.force_keyframe);
    assert_eq!(opts.ltr_acknowledged, None, "an ack is passed once");
    assert!(encoder.complete(refresh));
    s.pump();
    let header = *s.out.samples().last().expect("the refresh");
    assert_eq!(
        (header.sequence, header.flags),
        (refresh, 0),
        "a delta, not an IDR"
    );
    s.out.finish(header, true);
    s.pump();
    let stats = s.controller.stats();
    assert_eq!(
        (
            stats.recoveries,
            stats.ltr_refreshes,
            stats.ltr_refresh_failed
        ),
        (1, 1, 0)
    );
    // Written: the recovery is over; nothing more is forced.
    s.advance_ms(1_000);
    assert_eq!(s.controller.stats().recoveries, 1);
    let next = deliver(&s, 5_000);
    let opts = submit(&s, next);
    assert!(!opts.ltr_refresh && !opts.force_keyframe);
    s.close();
    s.assert_clean();
}

#[test]
fn an_ltr_refresh_is_not_held_back_by_the_idr_interval() {
    let s = ladder(&LTR, |_, _| {});
    acked(&s);
    for round in 0..3u64 {
        let lost = lose_next(&s, 10_000 + round * 2_000);
        let refresh = lost + 1;
        assert!(submit(&s, refresh).ltr_refresh, "round {round}");
        assert!(s.encoder().complete(refresh));
        s.pump();
        s.out
            .finish(*s.out.samples().last().expect("refresh"), true);
        s.pump();
    }
    let stats = s.controller.stats();
    assert_eq!((stats.ltr_refreshes, stats.recoveries), (3, 3));
    s.close();
    s.assert_clean();
}

#[test]
fn without_an_acknowledgement_the_recovery_is_an_idr() {
    let s = ladder(&LTR, |_, _| {});
    deliver(&s, 1_000);
    deliver(&s, 2_000);
    let lost = lose_next(&s, 3_000);
    let opts = submit(&s, lost + 1);
    assert!(opts.force_keyframe && !opts.ltr_refresh);
    assert_eq!(s.controller.stats().ltr_refreshes, 0);
    s.encoder().complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn without_confirmed_caps_acks_are_ignored_and_recovery_is_an_idr() {
    let s = ladder(&[RecoveryStep::Idr], |_, _| {});
    assert!(
        !s.encoder_after(|_| {}).spec.ltr,
        "the encoder is not asked for LTR"
    );
    deliver(&s, 1_000);
    s.controller.note_ltr_ack(generation(&s), 1);
    s.pump();
    let stats = s.controller.stats();
    assert_eq!((stats.ltr_acks, stats.ltr_acks_ignored), (0, 1));
    let lost = lose_next(&s, 2_000);
    assert!(submit(&s, lost + 1).force_keyframe);
    s.encoder().complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn acks_of_another_generation_or_an_unmarked_token_are_ignored() {
    let s = ladder(&LTR, |_, _| {});
    assert!(s.encoder().spec.ltr, "the encoder is asked for LTR");
    deliver(&s, 1_000);
    deliver(&s, 2_000);
    let wire = generation(&s);
    s.controller.note_ltr_ack(wire + 1, 2);
    s.controller.note_ltr_ack(wire, 99);
    s.controller.note_ltr_ack(wire, 0);
    s.controller.note_ltr_ack(0, 2);
    s.pump();
    let stats = s.controller.stats();
    assert_eq!((stats.ltr_acks, stats.ltr_acks_ignored), (0, 4));
    // A repeated or older ack changes nothing and is not an error.
    s.controller.note_ltr_ack(wire, 2);
    s.controller.note_ltr_ack(wire, 2);
    s.controller.note_ltr_ack(wire, 1);
    s.pump();
    let stats = s.controller.stats();
    assert_eq!((stats.ltr_acks, stats.ltr_acks_ignored), (1, 4));
    let lost = lose_next(&s, 3_000);
    assert!(submit(&s, lost + 1).ltr_refresh);
    s.encoder().complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn a_new_encoder_generation_clears_the_acknowledgement() {
    let s = ladder(&LTR, |_, _| {});
    acked(&s);
    let first = s.encoder();
    // The session dies and is rebuilt: a new encoder and wire generation.
    first.fail_session(MediaError::EncoderFailed { recoverable: true }, true);
    s.pump();
    s.frame(4_000);
    let second = s.encoder();
    assert_ne!(first.index, second.index);
    second.complete_all();
    s.pump();
    s.out
        .finish(*s.out.samples().last().expect("new IDR"), true);
    s.pump();
    s.advance_ms(600);
    // The old ack names a token of the old generation: ignored.
    let old_generation = s.out.samples()[0].generation;
    assert_ne!(old_generation, generation(&s));
    s.controller.note_ltr_ack(old_generation, 2);
    s.pump();
    assert_eq!(s.controller.stats().ltr_acks_ignored, 1);
    let lost = lose_next(&s, 5_000);
    let opts = submit(&s, lost + 1);
    assert!(opts.force_keyframe && !opts.ltr_refresh);
    s.encoder().complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn a_transport_change_clears_the_acknowledgement() {
    let s = ladder(&LTR, |_, _| {});
    acked(&s);
    s.controller.note_transport_changed();
    s.pump();
    // Acks of frames sent before the change no longer count.
    s.controller.note_ltr_ack(generation(&s), 3);
    s.pump();
    assert_eq!(s.controller.stats().ltr_acks_ignored, 1);
    let lost = lose_next(&s, 4_000);
    assert!(submit(&s, lost + 1).force_keyframe);
    s.encoder().complete_all();
    s.pump();
    s.out.finish(*s.out.samples().last().expect("IDR"), true);
    s.pump();
    // A frame sent on the new transport, acknowledged: LTR again.
    let seq = deliver(&s, 5_000);
    s.controller
        .note_ltr_ack(generation(&s), u32::try_from(seq).unwrap());
    s.pump();
    deliver(&s, 6_000);
    let lost = lose_next(&s, 7_000);
    assert!(submit(&s, lost + 1).ltr_refresh);
    s.encoder().complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn a_refresh_the_encoder_does_not_honour_escalates_to_an_idr_at_once() {
    let s = ladder(&LTR, |_, _| {});
    acked(&s);
    let encoder = s.encoder();
    encoder
        .ignore_refresh
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let lost = lose_next(&s, 4_000);
    let refresh = lost + 1;
    assert!(submit(&s, refresh).ltr_refresh);
    // A plain delta comes out: not a recovery.
    assert!(encoder.complete(refresh));
    s.pump();
    let idr = refresh + 1;
    assert_eq!(encoder.pending_seqs(), vec![idr], "the IDR follows at once");
    let opts = submit(&s, idr);
    assert!(opts.force_keyframe && !opts.ltr_refresh);
    let stats = s.controller.stats();
    assert_eq!(
        (
            stats.ltr_refreshes,
            stats.ltr_refresh_failed,
            stats.recoveries
        ),
        (1, 1, 2)
    );
    encoder.complete_all();
    s.pump();
    let last = *s.out.samples().last().expect("IDR");
    assert_eq!(last.flags, 1);
    s.out.finish(last, true);
    s.pump();
    s.advance_ms(1_000);
    assert_eq!(s.controller.stats().recoveries, 2);
    s.close();
    s.assert_clean();
}

#[test]
fn a_lost_refresh_escalates_and_later_idrs_stay_at_two_per_second() {
    let s = ladder(&LTR, |_, _| {});
    acked(&s);
    let encoder = s.encoder();
    let lost = lose_next(&s, 4_000);
    let refresh = lost + 1;
    assert!(submit(&s, refresh).ltr_refresh);
    // The refresh itself is lost (refused by the transport).
    assert!(encoder.complete(refresh));
    s.pump();
    s.out
        .finish(*s.out.samples().last().expect("refresh"), false);
    s.pump();
    let idr = refresh + 1;
    assert_eq!(
        encoder.pending_seqs(),
        vec![idr],
        "the first IDR is immediate"
    );
    assert!(submit(&s, idr).force_keyframe);
    // That IDR is lost too: the next IDR waits for the 500 ms interval.
    assert!(encoder.drop_output(idr, DropReason::NoOutput));
    s.pump();
    s.advance_ms(400);
    assert!(encoder.pending_seqs().is_empty());
    s.advance_ms(110);
    assert_eq!(encoder.pending_seqs(), vec![idr + 1]);
    assert!(submit(&s, idr + 1).force_keyframe);
    let stats = s.controller.stats();
    assert_eq!(
        (
            stats.ltr_refreshes,
            stats.ltr_refresh_failed,
            stats.recoveries
        ),
        (1, 1, 3)
    );
    encoder.complete_all();
    s.pump();
    s.out.finish(*s.out.samples().last().expect("IDR"), true);
    s.pump();
    // After the episode ends, the held ack makes the next recovery a refresh.
    let lost = lose_next(&s, 9_000);
    assert!(submit(&s, lost + 1).ltr_refresh);
    encoder.complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn a_client_keyframe_request_gets_an_idr_and_clears_the_acknowledgement() {
    let s = ladder(&LTR, |_, _| {});
    acked(&s);
    s.controller.request_keyframe();
    s.pump();
    let encoder = s.encoder();
    let seq = *encoder.pending_seqs().last().expect("IDR");
    assert!(submit(&s, seq).force_keyframe);
    encoder.complete_all();
    s.pump();
    s.out.finish(*s.out.samples().last().expect("IDR"), true);
    s.pump();
    // The client's decoder may be new: its old ack is gone.
    let lost = lose_next(&s, 5_000);
    s.advance_ms(600);
    let opts = submit(&s, lost + 1);
    assert!(opts.force_keyframe && !opts.ltr_refresh);
    encoder.complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn the_refresh_and_marks_reach_the_wire_as_ltr_tlvs() {
    let s = ladder(&LTR, |fakes, _| {
        fakes
            .out
            .frames_v4
            .store(true, std::sync::atomic::Ordering::SeqCst);
    });
    acked(&s);
    let lost = lose_next(&s, 4_000);
    s.encoder().complete_all();
    s.pump();
    let packets: Vec<_> = s
        .out
        .packets()
        .into_iter()
        .filter(|p| !p.meta.config)
        .collect();
    let mark = packets[1].meta.extensions.ltr.expect("frame 2 marks");
    assert_eq!((mark.token, mark.flags), (2, 0x01));
    let refresh = packets.last().expect("refresh");
    assert_eq!(refresh.meta.header.sequence, lost + 1);
    let ltr = refresh.meta.extensions.ltr.expect("the refresh TLV");
    assert_eq!((ltr.token, ltr.flags), (2, 0x02), "predicted from token 2");
    s.close();
    s.assert_clean();
}

#[test]
fn the_ack_and_the_refresh_reach_the_encoder_through_the_c_abi() {
    let s = Manual::with(|_, parts| {
        parts.plan.pacing.refresh_aligned = false;
        parts.plan.recovery = LTR.to_vec();
        parts.plan.extensions = vec![FrameExtension::Ltr];
    });
    s.factory
        .native
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let callbacks = native::StreamCallbacks {
        queue: std::sync::Arc::as_ptr(&s.queue).cast_mut().cast(),
        wake: Some(glue::wake),
        capture: std::sync::Arc::as_ptr(&s.capture).cast_mut().cast(),
        release: Some(glue::release),
    };
    assert!(native::streamcore_video_attach_stream(
        s.queue.port.native_key(),
        callbacks
    ));
    s.start_running();
    acked(&s);
    let lost = lose_next(&s, 4_000);
    let opts = submit(&s, lost + 1);
    assert!(opts.ltr_refresh && !opts.force_keyframe);
    s.encoder().complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn the_planner_takes_ltr_and_layers_only_from_confirmed_caps_and_willing_clients() {
    let all = encoder_plan_features(Some(confirmed_caps()), true, true);
    assert_eq!(all.recovery, LTR.to_vec());
    assert_eq!(
        all.extensions,
        vec![
            FrameExtension::Discardable,
            FrameExtension::Ltr,
            FrameExtension::TemporalId
        ]
    );
    for support in [FeatureSupport::Requested, FeatureSupport::Unsupported] {
        let caps = EncoderCaps {
            ltr: support,
            temporal_layers: support,
            ..confirmed_caps()
        };
        let features = encoder_plan_features(Some(caps), true, true);
        assert_eq!(features.recovery, vec![RecoveryStep::Idr]);
        assert!(features.extensions.is_empty());
    }
    let none = encoder_plan_features(None, true, true);
    assert_eq!(none.recovery, vec![RecoveryStep::Idr]);
    let no_client = encoder_plan_features(Some(confirmed_caps()), false, false);
    assert_eq!(no_client.recovery, vec![RecoveryStep::Idr]);
    assert!(no_client.extensions.is_empty());
    let ltr_only = encoder_plan_features(Some(confirmed_caps()), true, false);
    assert_eq!(ltr_only.extensions, vec![FrameExtension::Ltr]);
    let layers_only = encoder_plan_features(Some(confirmed_caps()), false, true);
    assert_eq!(layers_only.recovery, vec![RecoveryStep::Idr]);
    assert_eq!(
        layers_only.extensions,
        vec![FrameExtension::Discardable, FrameExtension::TemporalId]
    );
}

// ---------------------------------------------------------------------------
// Two temporal layers through the bounded sink (6.4).

fn bounded_policy(delta_age_bound: bool) -> SinkPolicy {
    SinkPolicy {
        timing_every: 0,
        frame_timing_every: 0,
        delta_age_bound,
        ..SinkPolicy::default()
    }
}

/// A stream whose plan comes from `encoder_plan_features(caps)` and whose
/// sink runs its bounds.
fn layered(caps: Option<EncoderCaps>, delta_age_bound: bool) -> Manual {
    let features = encoder_plan_features(caps, true, true);
    ladder(&features.recovery.clone(), move |fakes, parts| {
        parts.plan.extensions = features.extensions;
        parts.output =
            VideoOutput::with_transport(fakes.out.clone(), bounded_policy(delta_age_bound), None);
    })
}

/// Completes `seq` as a two-layer encoder does: odd frames after the IDR on
/// layer 1, discardable.
fn complete_layered(encoder: &FakeEncoder, seq: u64) {
    let upper = seq.is_multiple_of(2);
    assert!(encoder.complete_with(seq, BitstreamForm::Avcc4, upper, Some(u8::from(upper))));
}

#[test]
fn discardable_layer_1_frames_are_shed_without_recovery() {
    let s = layered(Some(confirmed_caps()), false);
    assert!(s.encoder_after(|_| {}).spec.temporal_layers);
    let encoder = s.encoder();
    // Frame 1 (IDR) in the transport; 2 (layer 1) and 3 (layer 0) queue.
    s.frame(1_000);
    s.frame(2_000);
    s.frame(3_000);
    for seq in 1..=3 {
        complete_layered(&encoder, seq);
    }
    s.pump();
    while s.out.finish_any(0, true) {}
    s.pump();
    let sent: Vec<_> = s.out.samples().iter().map(|h| h.sequence).collect();
    assert_eq!(sent, vec![1, 3], "layer-1 frame 2 shed by rule 1");
    let stats = s.controller.stats();
    assert_eq!(stats.sink_discardable_shed, 1);
    assert_eq!(
        (stats.sender_shed, stats.recoveries),
        (0, 0),
        "no chain break"
    );
    s.close();
    s.assert_clean();
}

#[test]
fn without_confirmed_layers_nothing_is_shed_and_the_age_bound_is_off() {
    let caps = EncoderCaps {
        ltr: FeatureSupport::Requested,
        temporal_layers: FeatureSupport::Requested,
        ..confirmed_caps()
    };
    let s = layered(Some(caps), true);
    assert!(!s.encoder_after(|_| {}).spec.temporal_layers);
    let encoder = s.encoder();
    assert!(!s.controller.stats().sink_age_bound);
    s.frame(1_000);
    s.frame(2_000);
    s.frame(3_000);
    for seq in 1..=3 {
        complete_layered(&encoder, seq);
    }
    s.pump();
    s.clock.advance(150 * MS);
    while s.out.finish_any(0, true) {}
    s.pump();
    let sent: Vec<_> = s.out.samples().iter().map(|h| h.sequence).collect();
    assert_eq!(sent, vec![1, 2, 3]);
    let stats = s.controller.stats();
    assert_eq!((stats.sink_discardable_shed, stats.sink_aged_shed), (0, 0));
    s.close();
    s.assert_clean();
}

#[test]
fn an_aged_layer_0_delta_is_recovered_by_an_ltr_refresh() {
    let s = layered(Some(confirmed_caps()), true);
    assert!(s.controller.stats().sink_age_bound);
    let encoder = s.encoder();
    // Three frames in the encoder; the IDR (token 1) is written and acknowledged.
    s.frame(1_000);
    s.frame(2_000);
    s.frame(3_000);
    complete_layered(&encoder, 1);
    s.pump();
    s.out.finish(s.out.samples()[0], true);
    s.pump();
    s.controller.note_ltr_ack(generation(&s), 1);
    s.frame(4_000);
    assert_eq!(encoder.pending_seqs(), vec![2, 3, 4]);
    // Frame 2 (layer 1) goes into the transport; frame 3 (layer 0) queues and
    // ages past 100 ms before frame 4 arrives.
    complete_layered(&encoder, 2);
    complete_layered(&encoder, 3);
    s.pump();
    s.clock.advance(150 * MS);
    complete_layered(&encoder, 4);
    s.pump();
    let stats = s.controller.stats();
    assert_eq!(stats.sink_aged_shed, 1, "frame 3 aged out");
    assert_eq!(stats.sender_shed, 1);
    // The shed delta is a loss. Recovery waits while the sink is
    // backpressured (6.5) and runs as soon as the sink is ready again, not at
    // the 500 ms re-check: an LTR refresh, not an IDR.
    assert_eq!(s.controller.stats().recoveries, 0);
    while s.out.finish_any(0, true) {}
    s.pump();
    let stats = s.controller.stats();
    assert_eq!((stats.recoveries, stats.recoveries_on_ready), (1, 1));
    let refresh = *encoder.pending_seqs().last().expect("recovery submitted");
    let opts = submit(&s, refresh);
    assert!(opts.ltr_refresh && !opts.force_keyframe);
    assert!(encoder.complete_with(refresh, BitstreamForm::Avcc4, false, Some(0)));
    s.pump();
    while s.out.finish_any(0, true) {}
    s.pump();
    let stats = s.controller.stats();
    assert_eq!((stats.ltr_refreshes, stats.recoveries), (1, 1));
    let sent: Vec<_> = s.out.samples().iter().map(|h| h.sequence).collect();
    assert_eq!(sent, vec![1, 2, 4, refresh]);
    assert_eq!(
        s.out.samples().iter().filter(|h| h.flags == 1).count(),
        1,
        "one IDR only"
    );
    s.close();
    s.assert_clean();
}
