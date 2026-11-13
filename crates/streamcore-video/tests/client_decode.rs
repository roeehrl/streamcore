//! `ClientStream`'s decode policy on a fake decoder (STREAMCORE-VIDEO.md 5,
//! 6.2, 6.5, 9.4): configuration and sessions, the queue bound, busy
//! decoders, stalls and failures, keyframe requests and their rate limit, LTR
//! acknowledgement and wait, feedback and close.

mod client_support;
mod support;

use client_support::*;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use streamcore_media::media::LtrInfo;
use streamcore_video::*;
use support::{MS, ThreadQueue, lock};

// ---------------------------------------------------------------------------
// Configuration and decode.

#[test]
fn the_first_config_packet_configures_the_decoder() {
    let c = Client::new();
    assert_eq!(c.stream.phase(), ClientPhase::Waiting);
    c.config(5);
    assert_eq!(
        lock(&c.decoder.calls)[..2],
        [
            Call::Configure {
                session: 1,
                config: b"config-5".to_vec(),
                color: ColorTags::BT709,
            },
            Call::SetMode(PresentMode::Responsive),
        ]
    );
    assert_eq!(c.stream.phase(), ClientPhase::Running);
    assert_eq!(
        c.jobs.count(
            |j| matches!(j, ClientJob::Configured { generation: 5, info, .. } if info.hardware)
        ),
        1
    );
    // A repeated configuration of the same generation (before each keyframe)
    // is not a new session.
    c.config(5);
    assert_eq!(c.decoder.configures(), 1);
    assert_eq!(c.stats().sessions, 1);
}

#[test]
fn each_generation_configures_with_its_own_colour() {
    let c = Client::new();
    let colors = || -> Vec<ColorTags> {
        lock(&c.decoder.calls)
            .iter()
            .filter_map(|call| match call {
                Call::Configure { color, .. } => Some(*color),
                _ => None,
            })
            .collect()
    };
    // BT.709, limited range, stated in the SPS's VUI.
    c.config_bytes(5, &avcc_with_signal(false, Some((1, 1, 1))));
    // BT.2020 PQ, full range.
    c.config_bytes(6, &avcc_with_signal(true, Some((9, 16, 9))));
    // A range without a colour description: the rest from the stream's
    // default; then a configuration without a VUI at all.
    c.config_bytes(7, &avcc_with_signal(true, None));
    c.config(8);
    let pq = ColorTags {
        primaries: ColorPrimaries::Bt2020,
        transfer: TransferFunction::Pq,
        matrix: YCbCrMatrix::Bt2020,
        full_range: true,
    };
    let full_709 = ColorTags {
        full_range: true,
        ..ColorTags::BT709
    };
    assert_eq!(colors(), [ColorTags::BT709, pq, full_709, ColorTags::BT709]);
    assert_eq!(
        c.jobs.count(
            |j| matches!(j, ClientJob::Configured { generation: 6, color, .. } if *color == pq)
        ),
        1
    );
    // A re-creation keeps the generation's colour.
    c.config_bytes(9, &avcc_with_signal(true, Some((9, 16, 9))));
    c.key(9, 1);
    c.advance_ms(500);
    assert_eq!(c.stats().stalls, 1);
    assert_eq!(colors().last(), Some(&pq));
    assert_eq!(c.decoder.configures(), 6);
}

#[test]
fn the_effective_queue_bound_and_deep_decoders_are_reported() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    c.output(1);
    assert_eq!(
        (c.stats().queue_bound, c.stats().deep_decoder_sessions),
        (2, 0)
    );
    *lock(&c.decoder.max_queued) = 5;
    c.config(6);
    assert_eq!(
        (c.stats().queue_bound, c.stats().deep_decoder_sessions),
        (6, 1)
    );
    // The depth in effect: declared 5 before any output.
    assert_eq!((c.stats().depth, c.stats().learned_depth), (5, 0));
}

#[test]
fn frames_are_decoded_in_order_with_their_metadata() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    c.output(1);
    c.delta(5, 2);
    c.output(2);
    c.sample(
        5,
        3,
        false,
        streamcore_media::media::VideoExtensions {
            discardable: true,
            temporal_id: 1,
            layout_epoch: Some(4),
            ..Default::default()
        },
    );
    let metas = c.decoder.decode_metas();
    assert_eq!(metas.iter().map(|m| m.seq).collect::<Vec<_>>(), [1, 2, 3]);
    assert!(metas[0].keyframe && !metas[1].keyframe);
    assert_eq!(metas[1].pts_ns, 2 * 16_667_000);
    assert_eq!(metas[1].generation, 5);
    assert!(metas[2].discardable);
    assert_eq!(metas[2].temporal_id, 1);
    assert_eq!(metas[2].layout_epoch, Some(4));
    assert!(!metas[1].discardable && metas[1].temporal_id == 0 && metas[1].ltr_token == 0);
    let stats = c.stats();
    assert_eq!((stats.submitted, stats.decoded, stats.presented), (3, 2, 2));
    assert!(c.jobs.keyframe_requests().is_empty());
}

#[test]
fn a_new_generation_flushes_then_configures_the_same_decoder_again() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    let old = c.decoder.port();
    c.config(6);
    assert_eq!(c.decoder.flushes(), 1);
    assert_eq!(c.decoder.configures(), 2);
    let calls = lock(&c.decoder.calls).clone();
    let flush = calls.iter().position(|c| *c == Call::Flush).unwrap();
    assert!(
        matches!(&calls[flush + 1], Call::Configure { session: 2, config, .. } if config == b"config-6")
    );
    // The old generation's waiting frame was dropped unshown.
    assert_eq!(c.stats().flushed, 1);
    // Its late output is stale.
    old.decoded(1, c.decoder.native_now());
    c.queue.pump();
    assert_eq!(c.stats().decoded, 0);
    assert_eq!(c.stats().stale_events, 1);
    // The new generation decodes from its keyframe.
    c.key(6, 1);
    c.output(1);
    assert_eq!(c.stats().decoded, 1);
    // A frame of the old generation is ignored.
    c.delta(5, 2);
    assert_eq!(c.decoder.decode_seqs(), [1, 1]);
    assert_eq!(c.stats().ignored, 1);
}

// ---------------------------------------------------------------------------
// Keyframe requests.

#[test]
fn a_gap_requests_a_keyframe_at_most_twice_per_second() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    c.output(1);
    c.delta(5, 3); // 2 is missing: a broken chain
    assert_eq!(c.jobs.keyframe_requests(), [KeyframeReason::Broken]);
    assert_eq!(c.stats().waiting_dropped, 1);
    // More broken frames inside the interval: no new request yet.
    c.advance_ms(100);
    c.delta(5, 4);
    c.delta(5, 5);
    assert_eq!(c.jobs.keyframe_requests().len(), 1);
    assert_eq!(c.stats().keyframe_requests_deferred, 1);
    // At the interval's end the chain is still broken: one more.
    c.advance_ms(390);
    assert_eq!(c.jobs.keyframe_requests().len(), 1);
    c.advance_ms(10);
    assert_eq!(c.jobs.keyframe_requests().len(), 2);
    // A keyframe restores it; a deferred request is dropped.
    c.delta(5, 6);
    c.key(5, 7);
    c.output(7);
    c.advance_ms(1_000);
    assert_eq!(c.jobs.keyframe_requests().len(), 2);
    assert_eq!(c.decoder.decode_seqs(), [1, 7]);
}

#[test]
fn a_frame_before_the_configuration_requests_a_keyframe() {
    let c = Client::new();
    c.delta(5, 9);
    assert_eq!(c.jobs.keyframe_requests(), [KeyframeReason::Broken]);
    assert_eq!(c.decoder.configures(), 0);
    c.config(5);
    c.key(5, 10);
    assert_eq!(c.decoder.decode_seqs(), [10]);
}

#[test]
fn more_than_two_waiting_frames_flush_and_request_a_keyframe() {
    let c = Client::new();
    c.config(5);
    // The first output teaches the stream the decoder's depth (1): bound
    // max(2, 1 + 1) = 2.
    c.key(5, 1);
    c.output(1);
    assert_eq!((c.stats().learned_depth, c.stats().queue_bound), (1, 2));
    c.delta(5, 2);
    c.delta(5, 3);
    // Two wait; a third delta flushes them and itself.
    c.delta(5, 4);
    assert_eq!(c.decoder.flushes(), 1);
    assert_eq!(c.decoder.decode_seqs(), [1, 2, 3]);
    assert_eq!(c.jobs.keyframe_requests(), [KeyframeReason::QueueBound]);
    let stats = c.stats();
    assert_eq!((stats.queue_flushes, stats.flushed), (1, 3));
    // Deltas until the keyframe are dropped (the request is not repeated
    // inside the interval).
    c.delta(5, 5);
    assert_eq!(c.decoder.decode_seqs(), [1, 2, 3]);
    c.key(5, 6);
    c.output(6);
    assert_eq!(c.decoder.decode_seqs(), [1, 2, 3, 6]);
    // Late outputs of the flushed frames name nothing.
    c.decoder.decoded(3);
    c.queue.pump();
    assert_eq!(c.stats().decoded, 2);
}

#[test]
fn until_the_first_output_the_bound_is_provisional() {
    let c = Client::new();
    c.config(5);
    assert_eq!(c.stats().queue_bound, 4);
    // Four inputs without an output are not a backlog yet.
    c.key(5, 1);
    for seq in 2..=4 {
        c.delta(5, seq);
    }
    assert_eq!(c.decoder.flushes(), 0);
    // A fifth is.
    c.delta(5, 5);
    assert_eq!(c.decoder.flushes(), 1);
}

#[test]
fn a_decoder_three_deep_runs_without_flushes() {
    let c = Client::new();
    c.config(5);
    // Output lags input by two frames: the first output after the third input.
    c.key(5, 1);
    c.delta(5, 2);
    c.delta(5, 3);
    c.output(1);
    for seq in 4..=120 {
        c.delta(5, seq);
        c.output(seq - 2);
        c.advance_ms(16);
    }
    let stats = c.stats();
    assert_eq!((stats.learned_depth, stats.depth), (3, 3));
    // One frame of slack beyond the pipeline.
    assert_eq!(stats.queue_bound, 4);
    assert_eq!(stats.deep_decoder_sessions, 1);
    assert_eq!((stats.queue_flushes, stats.flushed), (0, 0));
    assert!(c.jobs.keyframe_requests().is_empty());
    assert_eq!(stats.decoded, 118);
    // One frame of jitter: two arrive before the next output.
    c.delta(5, 121);
    c.delta(5, 122);
    c.output(119);
    c.output(120);
    assert_eq!(c.stats().queue_flushes, 0);
    assert_eq!(c.stats().decoded, 120);
    // A new session learns again.
    c.config(6);
    assert_eq!((c.stats().learned_depth, c.stats().queue_bound), (0, 4));
}

#[test]
fn a_keyframe_over_the_bound_flushes_without_a_request() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    c.output(1);
    c.delta(5, 2);
    c.delta(5, 3);
    c.key(5, 4);
    assert_eq!(c.decoder.flushes(), 1);
    assert_eq!(c.decoder.decode_seqs(), [1, 2, 3, 4]);
    assert!(c.jobs.keyframe_requests().is_empty());
    assert_eq!(c.stats().flushed, 2);
    c.output(4);
    c.delta(5, 5);
    assert_eq!(c.decoder.decode_seqs(), [1, 2, 3, 4, 5]);
}

#[test]
fn the_decoders_declared_depth_raises_the_bound() {
    let c = Client::new();
    *lock(&c.decoder.max_queued) = 4;
    c.config(5);
    // max(2, 4 + 1) = 5.
    for seq in 1..=5 {
        c.sample(5, seq, seq == 1, Default::default());
    }
    assert_eq!(c.decoder.flushes(), 0);
    c.delta(5, 6);
    assert_eq!(c.decoder.flushes(), 1);
}

// ---------------------------------------------------------------------------
// A busy decoder.

#[test]
fn a_busy_decoder_gets_the_waiting_frames_in_order_at_input_ready() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    c.output(1);
    c.decoder.answer(DecodeResult::Busy);
    c.delta(5, 2);
    assert_eq!(c.stats().busy, 1);
    // While busy, nothing is submitted; a newer frame waits behind the held one.
    c.delta(5, 3);
    assert_eq!(c.decoder.decode_seqs(), [1, 2]);
    assert_eq!(c.stats().flushed, 0);
    c.decoder.port().input_ready();
    c.queue.pump();
    // Both, in order: the chain is intact, so no keyframe is needed.
    assert_eq!(c.decoder.decode_seqs(), [1, 2, 2, 3]);
    assert!(c.jobs.keyframe_requests().is_empty());
    c.output(2);
    c.output(3);
    assert_eq!(c.stats().decoded, 3);
}

#[test]
fn input_ready_submits_until_the_decoder_is_busy_again() {
    let c = Client::with(|_| {});
    *lock(&c.decoder.max_queued) = 3;
    c.config(5);
    c.key(5, 1);
    c.output(1);
    // Bound max(2, 3 + 1) = 4.
    c.decoder.answer(DecodeResult::Busy);
    c.delta(5, 2);
    c.delta(5, 3);
    c.delta(5, 4);
    // The decoder takes one, then is busy again: 3 and 4 keep waiting.
    c.decoder.answer(DecodeResult::Accepted);
    c.decoder.answer(DecodeResult::Busy);
    c.decoder.port().input_ready();
    c.queue.pump();
    assert_eq!(c.decoder.decode_seqs(), [1, 2, 2, 3]);
    c.decoder.port().input_ready();
    c.queue.pump();
    assert_eq!(c.decoder.decode_seqs(), [1, 2, 2, 3, 3, 4]);
    assert_eq!(c.stats().flushed, 0);
    assert!(c.jobs.keyframe_requests().is_empty());
}

#[test]
fn a_keyframe_while_busy_replaces_the_waiting_frames() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    c.output(1);
    c.decoder.answer(DecodeResult::Busy);
    c.delta(5, 2);
    c.key(5, 3);
    assert_eq!(c.stats().flushed, 1);
    c.decoder.port().input_ready();
    c.queue.pump();
    assert_eq!(c.decoder.decode_seqs(), [1, 2, 3]);
    assert!(c.jobs.keyframe_requests().is_empty());
}

#[test]
fn a_full_wait_while_busy_is_a_queue_bound_flush() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    c.output(1);
    // Bound 2: one held and one more waiting, a third flushes.
    c.decoder.answer(DecodeResult::Busy);
    c.delta(5, 2);
    c.delta(5, 3);
    c.delta(5, 4);
    assert_eq!(c.stats().queue_flushes, 1);
    assert_eq!(c.jobs.keyframe_requests(), [KeyframeReason::QueueBound]);
}

#[test]
fn a_refused_frame_drops_it_and_requests_a_keyframe() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    c.output(1);
    c.decoder.answer(DecodeResult::Failed {
        error: MediaError::OutOfResources,
    });
    c.delta(5, 2);
    assert_eq!(c.jobs.keyframe_requests(), [KeyframeReason::DecodeFailed]);
    c.delta(5, 3);
    assert_eq!(c.decoder.decode_seqs(), [1, 2]);
    // DeviceLost re-creates the session instead.
    c.key(5, 4);
    c.output(4);
    c.decoder.answer(DecodeResult::Failed {
        error: MediaError::DeviceLost,
    });
    c.delta(5, 5);
    assert_eq!(c.decoder.configures(), 2);
    assert_eq!(
        c.jobs.count(|j| matches!(
            j,
            ClientJob::DecoderRecreated {
                reason: RecreateReason::Failed {
                    error: MediaError::DeviceLost
                }
            }
        )),
        1
    );
}

// ---------------------------------------------------------------------------
// Reentrancy and stale events.

#[test]
fn events_from_inside_decode_only_post() {
    let c = Client::new();
    *lock(&c.decoder.inline) = Inline::DecodeAndPresent;
    c.config(5);
    for seq in 1..=10 {
        c.sample(5, seq, seq == 1, Default::default());
    }
    let stats = c.stats();
    assert_eq!((stats.decoded, stats.presented), (10, 10));
    assert_eq!(stats.presented_estimated, 10);
    assert_eq!(stats.queue_flushes, 0);
    assert_eq!(stats.stale_events, 0);
    // A queue that drains inside `wake` (breaking "never inline") still
    // cannot re-enter the drain that is calling decode.
    c.queue.inline_wake.store(true, Ordering::SeqCst);
    c.delta(5, 11);
    c.queue.pump();
    assert_eq!(c.stats().decoded, 11);
    assert_eq!(c.decoder.off_executor.load(Ordering::SeqCst), 0);
}

#[test]
fn events_from_inside_decode_on_a_real_queue_thread() {
    let clock = Arc::new(support::ManualClock::default());
    let port = StreamPort::new();
    let queue = ThreadQueue::new(port.clone());
    let closed = Arc::new(AtomicBool::new(false));
    let decoder = FakeDecoder::new(clock.clone(), closed.clone());
    *lock(&decoder.inline) = Inline::DecodeAndPresent;
    let jobs = Arc::new(Jobs {
        jobs: Mutex::default(),
        closed,
    });
    let stream = ClientStream::with_transport(
        ClientParts {
            stream_id: STREAM,
            codec: Codec::H264,
            color: ColorTags::BT709,
            mode: PresentMode::Responsive,
            port,
            queue: queue.clone(),
            decoder: decoder.clone(),
            events: jobs.clone(),
            channel: None,
            timings: None,
        },
        clock,
        None,
    )
    .unwrap();
    let message = |metadata, data: &[u8]| {
        let packet = streamcore_media::media::ExtendedVideoPacket {
            metadata,
            extensions: Default::default(),
            data,
        }
        .encode(streamcore_media::media::FrameVersion::V4)
        .unwrap();
        streamcore_transport::channel::TimedMessage {
            payload: streamcore_media::binary::Frame {
                kind: 0x10,
                payload: &packet,
            }
            .encode()
            .unwrap(),
            first_byte_us: 1,
            complete_us: 1,
        }
    };
    let meta = |sequence, flags| streamcore_media::media::VideoMetadata {
        stream_id: STREAM,
        generation: 1,
        sequence,
        flags,
        pts_micros: if flags == 2 { 0 } else { sequence },
    };
    // Frames arrive on another thread while the queue thread drains.
    assert!(stream.receive(message(meta(0, 2), b"cfg")));
    for seq in 1..=200 {
        assert!(stream.receive(message(meta(seq, u8::from(seq == 1)), b"\0\0\0\x01\x41")));
    }
    queue.flush();
    queue.flush();
    let stats = stream.stats();
    assert_eq!(stats.decoded, 200);
    assert_eq!(decoder.off_executor.load(Ordering::SeqCst), 0);
    assert_eq!(stream.close_and_wait(5_000).unwrap(), CloseOutcome::Closed);
    queue.quit();
}

#[test]
fn events_of_a_replaced_session_are_ignored() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    let old = c.decoder.port();
    c.config(6);
    old.decoded(1, c.decoder.native_now());
    old.presented(1, c.decoder.native_now(), false);
    old.input_ready();
    old.failed(MediaError::DeviceLost, true);
    c.queue.pump();
    let stats = c.stats();
    assert_eq!(stats.stale_events, 4);
    assert_eq!(
        (stats.decoded, stats.recreates, stats.decoder_failures),
        (0, 0, 0)
    );
    assert_eq!(c.decoder.configures(), 2);
}

// ---------------------------------------------------------------------------
// Stalls and failures.

#[test]
fn a_decoder_without_output_for_500_ms_is_recreated() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    c.advance_ms(490);
    assert_eq!(c.decoder.configures(), 1);
    c.advance_ms(10);
    assert_eq!(c.stats().stalls, 1);
    assert_eq!(c.decoder.flushes(), 1);
    assert_eq!(c.decoder.configures(), 2);
    // Re-created with the generation's configuration.
    assert!(matches!(
        lock(&c.decoder.calls).iter().rev().find(|c| matches!(c, Call::Configure { .. })),
        Some(Call::Configure { session: 2, config, .. }) if config == b"config-5"
    ));
    assert_eq!(
        c.jobs.count(|j| *j
            == ClientJob::DecoderRecreated {
                reason: RecreateReason::Stalled
            }),
        1
    );
    assert_eq!(c.jobs.keyframe_requests(), [KeyframeReason::NewSession]);
    // The repeated configuration before the keyframe is not a new session.
    c.config(5);
    c.key(5, 2);
    c.output(2);
    assert_eq!(c.decoder.configures(), 2);
    assert_eq!(c.stats().decoded, 1);
}

#[test]
fn output_keeps_a_slow_decoder_alive() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    for seq in 2..=20 {
        c.advance_ms(300);
        c.decoder.decoded(seq - 1);
        c.queue.pump();
        c.delta(5, seq);
    }
    assert_eq!(c.stats().stalls, 0);
    // Idle with nothing waiting: no stall either.
    c.decoder.decoded(20);
    c.advance_ms(2_000);
    assert_eq!(c.stats().stalls, 0);
}

#[test]
fn a_recoverable_failure_recreates_and_another_fails_the_stream() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    c.output(1);
    c.decoder.port().failed(MediaError::OutOfResources, true);
    c.queue.pump();
    assert_eq!(c.decoder.configures(), 2);
    assert_eq!(c.stats().recreates, 1);
    assert_eq!(c.jobs.keyframe_requests(), [KeyframeReason::NewSession]);
    c.config(5);
    c.key(5, 2);
    c.output(2);
    c.decoder
        .port()
        .failed(MediaError::EncoderFailed { recoverable: false }, false);
    c.queue.pump();
    assert_eq!(c.stream.phase(), ClientPhase::Failed);
    assert_eq!(
        c.jobs.count(|j| matches!(
            j,
            ClientJob::Failed {
                error: MediaError::EncoderFailed { .. }
            }
        )),
        1
    );
    // Later frames are dropped without calling the decoder.
    let calls = lock(&c.decoder.calls).len();
    c.delta(5, 3);
    c.key(5, 4);
    assert_eq!(lock(&c.decoder.calls).len(), calls);
}

#[test]
fn more_than_three_recreations_in_ten_seconds_fail_the_stream() {
    let c = Client::new();
    c.config(5);
    for round in 0..4u64 {
        c.key(5, round * 2 + 1);
        c.decoder.port().failed(MediaError::DeviceLost, false);
        c.queue.pump();
        c.config(5);
        c.advance_ms(100);
    }
    assert_eq!(c.stream.phase(), ClientPhase::Failed);
    assert_eq!(c.stats().recreates, 4);
    assert_eq!(
        c.jobs.count(|j| *j
            == ClientJob::Failed {
                error: MediaError::DeviceLost
            }),
        1
    );
}

#[test]
fn an_unsupported_configuration_fails_the_stream() {
    let c = Client::new();
    lock(&c.decoder.configure_errors).push_back(MediaError::Unsupported);
    c.config(5);
    assert_eq!(c.stream.phase(), ClientPhase::Failed);
    assert_eq!(
        c.jobs.count(|j| *j
            == ClientJob::Failed {
                error: MediaError::Unsupported
            }),
        1
    );
}

#[test]
fn a_failed_configuration_waits_for_the_next_one() {
    let c = Client::new();
    lock(&c.decoder.configure_errors).push_back(MediaError::OutOfResources);
    c.config(5);
    assert_eq!(c.jobs.keyframe_requests(), [KeyframeReason::NewSession]);
    c.key(5, 1);
    assert!(c.decoder.decode_seqs().is_empty());
    // The host repeats its configuration before the keyframe.
    c.config(5);
    c.key(5, 2);
    assert_eq!(c.decoder.decode_seqs(), [2]);
    assert_eq!(c.decoder.configures(), 2);
}

#[test]
fn a_hung_flush_recreates_the_session() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    c.output(1);
    c.delta(5, 2);
    c.delta(5, 3);
    *lock(&c.decoder.flush_outcome) = InvalidateOutcome::TimedOut;
    c.delta(5, 4);
    let stats = c.stats();
    assert_eq!(stats.flush_timeouts, 1);
    assert_eq!(stats.recreates, 1);
    assert_eq!(c.decoder.configures(), 2);
    assert_eq!(
        c.jobs.count(|j| *j
            == ClientJob::DecoderRecreated {
                reason: RecreateReason::FlushTimedOut
            }),
        1
    );
}

// ---------------------------------------------------------------------------
// LTR (6.5).

#[test]
fn ltr_is_acknowledged_only_after_decode() {
    let c = Client::new();
    c.config(5);
    c.ltr(5, 1, true, 7, LtrInfo::MARK);
    // Submitted, not decoded: the report has no acknowledgement.
    c.advance_ms(100);
    let reports = c.feedback();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].ltr_ack, None);
    c.decoder.decoded(1);
    c.queue.pump();
    c.advance_ms(100);
    let reports = c.feedback();
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[1].ltr_ack, Some(7));
    assert_eq!(c.stats().ltr_acks, 1);
    // Sent once.
    c.delta(5, 2);
    c.advance_ms(200);
    assert!(c.feedback()[2..].iter().all(|r| r.ltr_ack.is_none()));
}

#[test]
fn while_ltr_is_in_use_a_break_waits_150_ms_for_the_refresh() {
    let c = Client::new();
    c.config(5);
    c.ltr(5, 1, true, 7, LtrInfo::MARK);
    c.output(1);
    c.delta(5, 2);
    c.output(2);
    c.delta(5, 4); // 3 missing
    assert!(c.jobs.keyframe_requests().is_empty());
    assert_eq!(c.stats().ltr_waits, 1);
    c.advance_ms(140);
    assert!(c.jobs.keyframe_requests().is_empty());
    c.advance_ms(10);
    assert_eq!(c.jobs.keyframe_requests(), [KeyframeReason::Broken]);
}

#[test]
fn an_ltr_refresh_inside_the_wait_restores_the_chain() {
    let c = Client::new();
    c.config(5);
    c.ltr(5, 1, true, 7, LtrInfo::MARK);
    c.output(1);
    c.delta(5, 3); // 2 missing
    c.advance_ms(50);
    c.ltr(5, 4, false, 7, LtrInfo::REFRESH);
    assert_eq!(c.decoder.decode_seqs(), [1, 4]);
    c.output(4);
    c.advance_ms(1_000);
    assert!(c.jobs.keyframe_requests().is_empty());
}

#[test]
fn without_an_acknowledged_ltr_a_break_requests_at_once() {
    let c = Client::new();
    c.config(5);
    // Marked but never decoded: not in use.
    c.ltr(5, 1, true, 7, LtrInfo::MARK);
    c.delta(5, 3);
    assert_eq!(c.jobs.keyframe_requests(), [KeyframeReason::Broken]);
    // A transport change forgets an acknowledged one.
    let c = Client::new();
    c.config(5);
    c.ltr(5, 1, true, 7, LtrInfo::MARK);
    c.output(1);
    c.stream.note_transport_changed();
    c.delta(5, 3);
    assert_eq!(c.jobs.keyframe_requests(), [KeyframeReason::Broken]);
}

// ---------------------------------------------------------------------------
// Feedback (9.4).

#[test]
fn feedback_goes_out_100_ms_after_a_change_and_never_when_idle() {
    let c = Client::new();
    c.config(5);
    let t0 = c.now_us();
    c.key(5, 1);
    c.advance_ms(10);
    c.output(1);
    c.advance_ms(80);
    assert!(c.feedback().is_empty());
    c.advance_ms(10);
    let reports = c.feedback();
    assert_eq!(reports.len(), 1);
    let report = &reports[0];
    assert_eq!(report.stream_id, STREAM);
    assert_eq!(report.generation, 5);
    assert_eq!(report.highest_complete_sequence, 1);
    assert_eq!(report.consumed_bytes, "config-5".len() as u64 + 8);
    assert_eq!(report.frames.len(), 1);
    let frame = report.frames[0];
    assert_eq!(frame.sequence, 1);
    assert_eq!(frame.receive_us, t0);
    // C3 and C5 from the decoder's clock, mapped onto the stream's.
    assert_eq!(frame.decode_us, t0 + 10_000);
    assert_eq!(frame.present_us, t0 + 10_000);
    // Nothing changes: nothing more is sent.
    c.advance_ms(1_000);
    assert_eq!(c.feedback().len(), 1);
}

#[test]
fn feedback_carries_at_most_32_frames_each_once() {
    let c = Client::new();
    *lock(&c.decoder.inline) = Inline::DecodeAndPresent;
    c.config(5);
    for seq in 1..=40 {
        c.sample(5, seq, seq == 1, Default::default());
    }
    c.advance_ms(100);
    let reports = c.feedback();
    assert_eq!(reports.len(), 1);
    let sequences: Vec<_> = reports[0].frames.iter().map(|f| f.sequence).collect();
    assert_eq!(sequences, (9..=40).collect::<Vec<_>>());
    assert!(
        reports[0]
            .frames
            .iter()
            .all(|f| f.decode_us != 0 && f.present_us != 0)
    );
    // Every C5 came from an estimating presenter.
    assert!(reports[0].c5_estimated);
    c.delta(5, 41);
    c.advance_ms(100);
    let reports = c.feedback();
    assert_eq!(reports.len(), 2);
    assert_eq!(
        reports[1]
            .frames
            .iter()
            .map(|f| f.sequence)
            .collect::<Vec<_>>(),
        [41]
    );
}

#[test]
fn feedback_reports_flushes_present_drops_and_loss() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    c.decoder.decoded(1);
    c.queue.pump();
    c.delta(5, 2);
    c.decoder.decoded(2);
    // 1 was decoded but replaced by 2 at the vsync.
    c.decoder.presented(2);
    c.queue.pump();
    // 3 and 4 never arrive; 5 is dropped while the chain is broken.
    c.delta(5, 5);
    c.advance_ms(100);
    let report = c.feedback().pop().unwrap();
    assert_eq!(report.present_drops, 1);
    assert_eq!(report.highest_complete_sequence, 5);
    let loss = report.loss.unwrap();
    assert_eq!(loss.base_sequence, 3);
    assert_eq!(loss.bits, [0b11]);
    let sequences: Vec<_> = report.frames.iter().map(|f| f.sequence).collect();
    assert_eq!(sequences, [1, 2, 5]);
    // 1 was decoded, never presented; 5 only received.
    assert!(report.frames[0].decode_us != 0 && report.frames[0].present_us == 0);
    assert!(report.frames[2].decode_us == 0);
    // A queue bound flush counts its frames.
    c.key(5, 6);
    c.delta(5, 7);
    c.delta(5, 8);
    c.advance_ms(100);
    let report = c.feedback().pop().unwrap();
    assert_eq!(report.flushes, 3);
    assert_eq!(report.loss, None);
}

#[test]
fn estimated_and_measured_present_times_never_share_a_report() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    c.decoder.decoded(1);
    c.decoder.port().presented(1, c.decoder.native_now(), true);
    c.queue.pump();
    c.delta(5, 2);
    c.decoder.decoded(2);
    c.decoder.port().presented(2, c.decoder.native_now(), true);
    c.queue.pump();
    c.delta(5, 3);
    c.output(3); // measured
    c.delta(5, 4); // not presented yet: reported with the first kind
    c.advance_ms(100);
    let reports = c.feedback();
    assert_eq!(reports.len(), 2);
    let sequences =
        |i: usize| -> Vec<u64> { reports[i].frames.iter().map(|f| f.sequence).collect() };
    assert!(reports[0].c5_estimated);
    assert_eq!(sequences(0), [1, 2, 4]);
    assert!(!reports[1].c5_estimated);
    assert_eq!(sequences(1), [3]);
    // The split report carries no loss bitmap or LTR ack, the same counters.
    assert!(reports[1].loss.is_none() && reports[1].ltr_ack.is_none());
    assert_eq!(reports[1].highest_complete_sequence, 4);
    assert_eq!(c.stats().presented_estimated, 2);
    // Measured only: one report, flag clear (4 was reported already).
    c.output(4);
    c.delta(5, 5);
    c.output(5);
    c.advance_ms(100);
    let reports = c.feedback();
    assert_eq!(reports.len(), 3);
    assert!(!reports[2].c5_estimated);
    assert_eq!(
        reports[2]
            .frames
            .iter()
            .map(|f| f.sequence)
            .collect::<Vec<_>>(),
        [5]
    );
}

#[test]
fn without_a_frames_v4_channel_feedback_goes_to_the_consumer() {
    let c = Client::new();
    c.lane0.v4.store(false, Ordering::SeqCst);
    c.config(5);
    c.key(5, 1);
    c.output(1);
    c.advance_ms(100);
    assert!(c.feedback().is_empty());
    let messages: Vec<_> = lock(&c.jobs.jobs)
        .iter()
        .filter_map(|j| match j {
            ClientJob::Feedback { message } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 1);
    assert_eq!(decode_feedback(&messages[0]).frames.len(), 1);
    assert_eq!(c.stats().feedback_posted, 1);
}

#[test]
fn a_new_generation_sends_the_old_ones_report_first() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    c.output(1);
    c.config(6);
    let reports = c.feedback();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].generation, 5);
    c.key(6, 1);
    c.output(1);
    c.advance_ms(100);
    let reports = c.feedback();
    assert_eq!(reports.last().unwrap().generation, 6);
}

// ---------------------------------------------------------------------------
// Commands and close.

#[test]
fn the_present_mode_reaches_every_session() {
    let c = Client::new();
    c.config(5);
    c.stream.set_mode(PresentMode::Smooth);
    c.queue.pump();
    c.config(6);
    let modes: Vec<_> = lock(&c.decoder.calls)
        .iter()
        .filter_map(|c| match c {
            Call::SetMode(mode) => Some(*mode),
            _ => None,
        })
        .collect();
    assert_eq!(
        modes,
        [
            PresentMode::Responsive,
            PresentMode::Smooth,
            PresentMode::Smooth
        ]
    );
}

#[test]
fn close_flushes_stops_timers_and_ignores_late_events() {
    let c = Client::new();
    c.config(5);
    c.key(5, 1);
    c.delta(5, 3); // a pending keyframe timer and a stall timer
    let port = c.decoder.port();
    c.stream.request_stats();
    c.stream.begin_close();
    c.queue.pump();
    assert_eq!(c.stream.phase(), ClientPhase::Closed);
    assert_eq!(c.stream.close_outcome(), Some(CloseOutcome::Closed));
    assert_eq!(c.decoder.flushes(), 1);
    assert_eq!(c.jobs.count(|j| matches!(j, ClientJob::Stats { .. })), 1);
    assert_eq!(
        c.jobs.count(|j| *j
            == ClientJob::Closed {
                outcome: CloseOutcome::Closed
            }),
        1
    );
    let jobs = lock(&c.jobs.jobs).len();
    port.decoded(1, 0);
    port.failed(MediaError::DeviceLost, true);
    port.input_ready();
    c.key(5, 4);
    c.advance_ms(2_000);
    assert_eq!(c.decoder.after_close.load(Ordering::SeqCst), 0);
    assert_eq!(lock(&c.jobs.jobs).len(), jobs);
}

#[test]
fn dropping_the_stream_closes_it() {
    let c = Client::new();
    c.config(5);
    let Client {
        stream,
        queue,
        decoder,
        jobs,
        ..
    } = c;
    let weak = Arc::downgrade(&stream);
    drop(stream);
    assert!(weak.upgrade().is_none());
    queue.pump();
    assert_eq!(decoder.flushes(), 1);
    assert_eq!(jobs.count(|j| matches!(j, ClientJob::Closed { .. })), 1);
}

#[test]
fn messages_of_other_kinds_or_streams_are_left_to_the_consumer() {
    let c = Client::new();
    let other = |kind: u8, payload: &[u8]| streamcore_transport::channel::TimedMessage {
        payload: streamcore_media::binary::Frame { kind, payload }
            .encode()
            .unwrap(),
        first_byte_us: 0,
        complete_us: 0,
    };
    assert!(!c.stream.receive(other(0x01, b"{}")));
    let mut packet = vec![0; 40];
    packet[..4].copy_from_slice(&(STREAM + 1).to_le_bytes());
    assert!(!c.stream.receive(other(0x10, &packet)));
    // This stream's, but malformed: taken and counted.
    let mut bad = vec![0; 40];
    bad[..4].copy_from_slice(&STREAM.to_le_bytes());
    bad[4] = 9;
    assert!(c.stream.receive(other(0x10, &bad)));
    c.queue.pump();
    assert_eq!(c.stats().invalid_frames, 1);
    let _ = MS;
}
