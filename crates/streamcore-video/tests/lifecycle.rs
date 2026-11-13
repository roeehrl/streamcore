//! The capture lifecycle (3.6), ported from the reference host's lifecycle
//! tests: serialized, token-fenced native operations with a timeout; a failed or
//! unknown stop retains the capture and blocks a replacement.

mod support;

use std::sync::atomic::Ordering;
use streamcore_video::*;
use support::*;

fn rejected(code: &str) -> OpOutcome {
    OpOutcome::Rejected { code: code.into() }
}

fn rate_update() -> StreamUpdate {
    StreamUpdate {
        rate: Some(RateUpdate {
            bitrate_bps: 1_000_000,
            fps: 30,
        }),
        capture: CaptureUpdate {
            min_interval_ns: Some(SECOND / 30),
            region: None,
            cursor: None,
        },
        encode_width: None,
        encode_height: None,
    }
}

#[test]
fn stop_during_start_waits_for_both_native_callbacks_and_blocks_replacement() {
    let s = Manual::new();
    let start = s.controller.start();
    s.pump();
    let stop = s.controller.stop();
    s.pump();
    assert_eq!(s.capture.count(Op::Stop), 0, "no stop overlaps the start");
    assert!(s.observer.results(start).is_empty());
    s.capture.answer(Op::Start, None);
    s.pump();
    assert_eq!(s.observer.results(start), vec![rejected(codes::CANCELLED)]);
    assert_eq!(s.capture.count(Op::Stop), 1);
    // A replacement waits for the stop.
    let replacement = s.controller.start();
    s.pump();
    assert_eq!(s.observer.results(replacement), vec![rejected(codes::BUSY)]);
    s.capture.answer(Op::Stop, None);
    s.pump();
    assert_eq!(s.observer.results(stop), vec![OpOutcome::Completed]);
    assert_eq!(s.controller.phase(), StreamPhase::Idle);
    s.start_running();
    s.close();
    s.assert_clean();
}

#[test]
fn update_and_stop_are_serialized_and_native_errors_remain_visible() {
    let s = Manual::new();
    s.start_running();
    let update = s.controller.update(rate_update());
    let second = s.controller.update(rate_update());
    let stop = s.controller.stop();
    s.pump();
    assert_eq!(s.observer.results(second), vec![rejected(codes::BUSY)]);
    assert_eq!(s.capture.count(Op::Stop), 0);
    s.capture.answer(Op::Update, Some(MediaError::Internal));
    s.pump();
    assert_eq!(s.observer.results(update), vec![rejected(codes::UPDATE)]);
    assert_eq!(s.capture.count(Op::Stop), 1);
    s.capture.answer(Op::Stop, Some(MediaError::Internal));
    s.pump();
    assert_eq!(s.observer.results(stop), vec![rejected(codes::STOP)]);
    assert_eq!(s.controller.phase(), StreamPhase::StopFailed);
    // A failed stop keeps the capture: another stop retries it.
    let retry = s.controller.stop();
    s.pump();
    assert_eq!(s.capture.count(Op::Stop), 2);
    s.capture.answer(Op::Stop, None);
    s.pump();
    assert_eq!(s.observer.results(retry), vec![OpOutcome::Completed]);
    s.close();
    s.assert_clean();
}

#[test]
fn a_missing_reply_is_unknown_and_retains_the_capture_until_a_confirmed_stop() {
    for updating in [false, true] {
        let s = Manual::new();
        let op = if updating {
            s.start_running();
            s.controller.update(rate_update())
        } else {
            s.controller.start()
        };
        s.pump();
        s.advance_ms(10_001);
        assert_eq!(s.observer.results(op), vec![OpOutcome::OutcomeUnknown]);
        let stop = s.controller.stop();
        let replacement = s.controller.start();
        s.pump();
        assert_eq!(s.observer.results(stop), vec![OpOutcome::OutcomeUnknown]);
        assert_eq!(s.observer.results(replacement), vec![rejected(codes::BUSY)]);
        assert_eq!(s.capture.count(Op::Stop), 0);
        // The late answer settles the native operation; the capture is stopped.
        s.capture
            .answer(if updating { Op::Update } else { Op::Start }, None);
        s.pump();
        assert_eq!(s.capture.count(Op::Stop), 1);
        assert_eq!(s.observer.results(op).len(), 1, "answered once");
        s.capture.answer(Op::Stop, None);
        s.pump();
        assert_eq!(s.controller.phase(), StreamPhase::Idle);
        s.start_running();
        s.close();
        s.assert_clean();
    }
}

#[test]
fn a_late_successful_stop_settles_the_capture_without_completing_the_request_twice() {
    let s = Manual::new();
    s.start_running();
    let stop = s.controller.stop();
    s.pump();
    s.advance_ms(10_001);
    assert_eq!(s.observer.results(stop), vec![OpOutcome::OutcomeUnknown]);
    assert_eq!(s.controller.phase(), StreamPhase::StopUnknown);
    s.capture.answer(Op::Stop, None);
    s.pump();
    assert_eq!(s.observer.results(stop), vec![OpOutcome::OutcomeUnknown]);
    assert_eq!(s.controller.phase(), StreamPhase::Idle);
    s.close();
    s.assert_clean();
}

#[test]
fn a_native_start_failure_is_reported_once_and_still_requires_cleanup() {
    let s = Manual::new();
    let start = s.controller.start();
    s.pump();
    s.capture
        .answer(Op::Start, Some(MediaError::PermissionDenied));
    s.pump();
    assert_eq!(s.observer.results(start), vec![rejected(codes::START)]);
    assert_eq!(s.capture.count(Op::Stop), 1);
    s.capture.answer(Op::Stop, None);
    s.pump();
    assert_eq!(s.observer.results(start).len(), 1);
    assert_eq!(s.controller.phase(), StreamPhase::Idle);
    s.close();
    s.assert_clean();
}

#[test]
fn revoked_ownership_cannot_start_or_revive_capture() {
    for revoke_before_start in [true, false] {
        let s = Manual::new();
        if revoke_before_start {
            s.controller.revoke();
        }
        let start = s.controller.start();
        s.pump();
        if revoke_before_start {
            assert_eq!(s.observer.results(start), vec![rejected(codes::CANCELLED)]);
            assert_eq!(s.capture.opens.load(Ordering::SeqCst), 0);
        } else {
            s.controller.revoke();
            s.capture.answer(Op::Start, None);
            s.pump();
            assert_eq!(s.observer.results(start), vec![rejected(codes::CANCELLED)]);
            assert_eq!(s.capture.count(Op::Stop), 1);
            s.capture.emit(srgb(), 1_000);
            s.pump();
            assert!(lock(&s.encoder().submits).is_empty() || s.factory.count() == 0);
        }
        // The sink is revoked: nothing is unsent and nothing more is sent.
        assert_eq!(s.controller.sink_state().unsent_frames, 0);
        assert!(s.out.samples().is_empty());
        s.close();
        s.assert_clean();
    }
}

#[test]
fn invalid_settings_are_refused_before_any_native_allocation() {
    let fakes = Fakes::new();
    let clock = std::sync::Arc::new(ManualClock::default());
    type Adjust = Box<dyn Fn(&mut StreamParts)>;
    let bad_plans: Vec<Adjust> = vec![
        Box::new(|p| p.plan.encode_width = 1),
        Box::new(|p| p.plan.encode_height = 8193),
        Box::new(|p| p.plan.fps = 0),
        Box::new(|p| p.plan.fps = 121),
        Box::new(|p| p.plan.permit_target = 4),
        Box::new(|p| p.plan.permit_target = 0),
        Box::new(|p| p.plan.bitrate_bps = 0),
        Box::new(|p| p.plan.codec = Codec::Av1),
        Box::new(|p| p.plan.convert = ConvertPath::Convert),
        Box::new(|p| p.plan.mode = StreamMode::View),
        Box::new(|p| p.spec.pixel_width = 0),
        Box::new(|p| p.spec.queue_depth = 0),
        Box::new(|p| {
            p.spec.target = CaptureTarget::Region {
                display_id: 1,
                rect: NormalizedRect {
                    x: 0.5,
                    y: 0.0,
                    width: 0.6,
                    height: 1.0,
                },
            }
        }),
        Box::new(|p| {
            p.spec.target = CaptureTarget::Region {
                display_id: 1,
                rect: NormalizedRect {
                    x: f64::NAN,
                    y: 0.0,
                    width: 0.5,
                    height: 0.5,
                },
            }
        }),
    ];
    for adjust in bad_plans {
        let port = StreamPort::new();
        let queue = ManualQueue::new(port.clone(), clock.clone());
        let mut parts = fakes.parts(port, queue, plan());
        adjust(&mut parts);
        assert!(StreamController::with_clock(parts, clock.clone()).is_err());
    }
    // A port binds to one stream only.
    let port = StreamPort::new();
    let queue = ManualQueue::new(port.clone(), clock.clone());
    let _first = StreamController::with_clock(
        fakes.parts(port.clone(), queue.clone(), plan()),
        clock.clone(),
    )
    .expect("valid");
    assert!(StreamController::with_clock(fakes.parts(port, queue, plan()), clock.clone()).is_err());
    assert_eq!(fakes.factory.creates.load(Ordering::SeqCst), 0);
    assert_eq!(fakes.capture.opens.load(Ordering::SeqCst), 0);
}

#[test]
fn hevc_falls_back_to_h264_once_and_the_announced_codec_is_the_one_created() {
    let s = Manual::with(|fakes, parts| {
        parts.plan.codec = Codec::Hevc;
        lock(&fakes.factory.unsupported).push(Codec::Hevc);
    });
    s.start_running();
    let started = s.observer.jobs().into_iter().find_map(|job| match job {
        ObserverJob::Started { plan, info } => Some((plan.codec, info.codec)),
        _ => None,
    });
    assert_eq!(started, Some((Codec::H264, Codec::H264)));
    // A replacement keeps the announced codec, with no fallback.
    s.capture.emit(p3(), 1_000);
    s.pump();
    assert_eq!(s.encoder().codec, Codec::H264);
    assert_eq!(s.factory.count(), 2);
    s.close();
    s.assert_clean();
}

#[test]
fn a_gone_source_fails_the_stream_and_stops_capture() {
    let s = Manual::new();
    s.start_running();
    let events = lock(&s.capture.events).clone().expect("open");
    events.source_gone(CAPTURE + 1, SourceGone::Closed);
    s.pump();
    assert!(
        s.observer.failures().is_empty(),
        "another capture's event is stale"
    );
    events.source_gone(CAPTURE, SourceGone::Closed);
    s.pump();
    assert_eq!(s.observer.failures(), vec![codes::CONTENT.to_string()]);
    assert_eq!(s.capture.count(Op::Stop), 1);
    s.capture.answer(Op::Stop, None);
    s.pump();
    assert_eq!(s.controller.phase(), StreamPhase::Idle);
    s.close();
    s.assert_clean();
}

#[test]
fn device_loss_rebuilds_the_encoder_until_it_happens_too_often() {
    let s = Manual::new();
    s.start_running();
    for n in 0..3u64 {
        s.frame(1_000 + n);
        let encoder = s.encoder();
        let seq = encoder.pending_seqs()[0];
        encoder.drop_output(
            seq,
            DropReason::Error {
                error: MediaError::DeviceLost,
            },
        );
        s.pump();
        assert_eq!(s.factory.count() as u64, 2 + n);
    }
    assert!(s.observer.failures().is_empty());
    s.frame(2_000);
    let encoder = s.encoder();
    let seq = encoder.pending_seqs()[0];
    encoder.drop_output(
        seq,
        DropReason::Error {
            error: MediaError::DeviceLost,
        },
    );
    s.pump();
    assert_eq!(s.observer.failures(), vec![codes::ENCODER.to_string()]);
    s.capture.answer(Op::Stop, None);
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn op_tokens_fence_stale_answers() {
    let s = Manual::new();
    s.start_running();
    let old = s.capture.last_op(Op::Start).expect("start");
    let stop = s.controller.stop();
    s.pump();
    // The old start's token answering again changes nothing.
    let events = lock(&s.capture.events).clone().expect("open");
    events.op_done(old, Some(MediaError::Internal));
    s.pump();
    assert_eq!(s.controller.phase(), StreamPhase::Stopping);
    s.capture.answer(Op::Stop, None);
    s.pump();
    assert_eq!(s.observer.results(stop), vec![OpOutcome::Completed]);
    assert!(s.controller.stats().stale_events >= 1);
    s.close();
    s.assert_clean();
}

#[test]
fn every_wire_code_is_documented_in_the_protocol_file() {
    let doc = include_str!("../../../protocol/STREAM-CODES.md");
    for code in codes::ALL {
        assert!(
            doc.contains(&format!("`{code}`")),
            "{code} is not in protocol/STREAM-CODES.md"
        );
    }
    let documented = doc.matches("| `stream.").count() + doc.matches("| `session.").count();
    assert_eq!(documented, codes::ALL.len());
}
