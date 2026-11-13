//! The 3.5 rules: inline frame drains, the deferred re-create, the bounded
//! invalidate, close, late callbacks and reentrancy.

mod support;

use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use streamcore_video::*;
use support::*;

fn no_capture_change() -> CaptureUpdate {
    CaptureUpdate {
        min_interval_ns: None,
        region: None,
        cursor: None,
    }
}

#[test]
fn a_frame_on_an_idle_queue_reaches_submit_with_no_hop() {
    let s = Manual::new();
    s.start_running();
    s.capture.emit(srgb(), 1_000);
    // No pump: admission and submit ran inside the capture callback.
    assert_eq!(s.encoder().pending_seqs(), vec![1]);
    s.close();
    s.assert_clean();
}

#[test]
fn a_stop_or_update_arriving_during_an_inline_drain_waits_for_the_next_wake() {
    for stop in [true, false] {
        let s = Manual::new();
        s.start_running();
        let encoder = s.encoder();
        let controller = s.controller.clone();
        let (tx, rx) = mpsc::channel();
        *lock(&encoder.on_submit) = Some(Box::new(move || {
            let op = if stop {
                controller.stop()
            } else {
                controller.update(StreamUpdate {
                    rate: Some(RateUpdate::new(1_000_000, 30)),
                    capture: no_capture_change(),
                    encode_width: None,
                    encode_height: None,
                })
            };
            tx.send(op).ok();
        }));
        s.capture.emit(srgb(), 1_000);
        let op = rx.recv().expect("command posted");
        assert_eq!(encoder.pending_seqs(), vec![1]);
        // Nothing called back into the backend from its own callback.
        assert_eq!(s.capture.count(Op::Stop) + s.capture.count(Op::Update), 0);
        assert!(lock(&encoder.rates).is_empty());
        *lock(&encoder.on_submit) = None;
        s.pump();
        if stop {
            assert_eq!(s.capture.count(Op::Stop), 1);
            assert_eq!(encoder.invalidated.load(Ordering::SeqCst), 1);
            s.capture.answer(Op::Stop, None);
            s.pump();
            assert_eq!(s.observer.results(op), vec![OpOutcome::Completed]);
            assert_eq!(s.controller.phase(), StreamPhase::Idle);
        } else {
            assert_eq!(s.capture.count(Op::Update), 1);
            assert_eq!(lock(&encoder.rates).len(), 1);
            s.capture.answer(Op::Update, None);
            s.pump();
            assert_eq!(s.observer.results(op), vec![OpOutcome::Completed]);
        }
        // The in-flight output of the stopped session is stale; it still
        // releases its permit.
        encoder.complete(1);
        s.pump();
        s.close();
        s.assert_clean();
    }
}

#[test]
fn a_colour_change_recreates_the_encoder_on_the_next_wake_never_inside_submit() {
    let s = Manual::new();
    s.start_running();
    let first = s.encoder();
    s.frame(1_000);
    first.complete(1);
    s.pump();
    assert_eq!(s.factory.count(), 1);

    // A P3 frame arrives inline on an sRGB session.
    s.capture.emit(p3(), 2_000);
    assert_eq!(s.factory.count(), 1, "no create inside the frame callback");
    assert_eq!(
        first.invalidated.load(Ordering::SeqCst),
        0,
        "no invalidate inline"
    );
    assert_eq!(
        lock(&first.submits).len(),
        1,
        "the frame is retained, not submitted"
    );
    s.pump();
    assert_eq!(first.invalidated.load(Ordering::SeqCst), 1);
    assert_eq!(s.factory.count(), 2);
    let second = s.encoder();
    assert_eq!(second.spec.color, p3());
    let submits = lock(&second.submits).clone();
    assert_eq!(submits.len(), 1);
    assert!(
        submits[0].force_keyframe,
        "the retained frame goes out as an IDR"
    );
    second.complete(submits[0].submit_seq);
    s.pump();

    // The new session's config is byte-identical, yet it starts a new
    // generation with config first.
    let packets = s.out.packets();
    let firsts: Vec<_> = packets.iter().map(|p| p.meta.header.generation).collect();
    assert!(firsts[firsts.len() - 1] > firsts[0]);
    let tail = &packets[packets.len() - 2..];
    assert!(tail[0].meta.config && tail[1].meta.keyframe);
    assert_eq!(s.controller.stats().encoders_created, 2);
    s.close();
    s.assert_clean();
    s.assert_wire_order();
}

#[test]
fn an_srgb_source_on_an_android_mapped_plan_gives_exactly_one_encoder_session() {
    let s = Manual::with(|_, parts| {
        parts.plan.color_mapping = ColorMapping {
            allow_p3: false,
            srgb_transfer_as_bt709: true,
        };
        parts.plan.color_out = ColorTags::BT709;
    });
    s.start_running();
    let bt709_source = ColorTags {
        transfer: TransferFunction::Bt709,
        ..srgb()
    };
    for (n, color) in [srgb(), bt709_source, p3(), srgb()].into_iter().enumerate() {
        s.capture.emit(color, 1_000 * (n as u64 + 1));
        s.pump();
        s.encoder().complete_all();
        s.pump();
    }
    assert_eq!(s.factory.count(), 1);
    assert_eq!(s.encoder().spec.color, ColorTags::BT709);
    assert_eq!(s.out.samples().len(), 4);
    s.close();
    s.assert_clean();
}

#[test]
fn late_callbacks_after_a_generation_change_are_ignored_and_still_release_permits() {
    let s = Manual::new();
    s.start_running();
    let first = s.encoder();
    for n in 1..=2 {
        s.frame(n * 1_000);
    }
    s.frame(2_500);
    assert_eq!(first.pending_seqs(), vec![1, 2, 3]);
    // Three permits are held natively; a fourth frame waits in the latest slot.
    s.frame(3_000);
    assert_eq!(lock(&first.submits).len(), 3);

    // Replace the session: the old permits still count against admission.
    s.capture.emit(p3(), 4_000);
    s.pump();
    let second = s.encoder();
    assert!(second.pending_seqs().is_empty(), "old permits still count");

    // Late outputs of the old session: discarded, permits released, and the
    // retained frame is admitted on the new session.
    first.complete(1);
    s.pump();
    assert_eq!(second.pending_seqs().len(), 1);
    first.drop_output(2, DropReason::NoOutput);
    first.complete(3);
    s.pump();
    assert!(
        s.out.packets().is_empty(),
        "nothing from the old session is sent"
    );
    assert!(s.controller.stats().stale_events >= 3);
    assert_eq!(s.log.native_permits.load(Ordering::SeqCst), 1);
    s.close();
    s.assert_clean();
}

#[test]
fn late_callbacks_after_close_are_dropped_on_the_posting_thread_and_release() {
    let s = Manual::new();
    s.start_running();
    let encoder = s.encoder();
    s.frame(1_000);
    let events = lock(&s.capture.events).clone().expect("open");
    s.close();
    assert!(!encoder.pending_seqs().is_empty());
    // An output, a drop and a frame after Closed.
    encoder.complete(1);
    let late = s.capture.surface(srgb(), 9_000);
    events.frame(CAPTURE, None, late, FrameStatus::Complete);
    events.op_done(OpToken { id: 99, epoch: 1 }, None);
    assert_eq!(s.log.native_permits.load(Ordering::SeqCst), 0);
    assert!(s.out.packets().is_empty());
    s.pump();
    s.assert_clean();
}

#[test]
fn a_busy_or_failed_submit_consumes_no_sequence_and_leaves_no_gap() {
    let s = Manual::new();
    s.start_running();
    let encoder = s.encoder();
    encoder.busy_next.store(true, Ordering::SeqCst);
    s.frame(1_000);
    assert!(encoder.pending_seqs().is_empty());
    // Busy: retried on the next output, drop or frame, not on its own release.
    s.frame(2_000);
    assert_eq!(encoder.pending_seqs(), vec![1]);
    *lock(&encoder.fail_next) = Some(MediaError::EncoderFailed { recoverable: true });
    s.frame(3_000);
    // The failed submit is a host-detected loss: recovered at once (6.5) by
    // re-sending the retained frame as an IDR, which takes the next sequence.
    assert_eq!(encoder.pending_seqs(), vec![1, 2]);
    assert!(lock(&encoder.submits)[1].force_keyframe);
    s.frame(4_000);
    assert_eq!(encoder.pending_seqs(), vec![1, 2, 3]);
    assert!(s.observer.failures().is_empty());
    // Free a permit for the next submit.
    encoder.complete(1);
    s.pump();
    *lock(&encoder.fail_next) = Some(MediaError::Internal);
    s.frame(5_000);
    assert_eq!(s.observer.failures(), vec![codes::ENCODE.to_string()]);
    s.close();
    s.assert_clean();
}

#[test]
fn a_stuck_invalidate_returns_within_its_bound_and_close_completes() {
    let fakes = Fakes::new();
    *lock(&fakes.factory.invalidate_mode) = InvalidateMode::Stuck;
    fakes.capture.auto_answer.store(true, Ordering::SeqCst);
    let port = StreamPort::new();
    let queue = ThreadQueue::new(port.clone());
    let controller =
        StreamController::new(fakes.parts(port, queue.clone(), plan())).expect("valid");
    controller.start();
    queue.flush();
    queue.flush();
    assert_eq!(controller.phase(), StreamPhase::Running);
    let capture = fakes.capture.clone();
    queue.run_on(move || {
        capture.emit(srgb(), 1_000);
    });
    queue.flush();
    let encoder = fakes.factory.last();
    assert_eq!(encoder.pending_seqs(), vec![1]);

    let started = Instant::now();
    let closer = controller.clone();
    let outcome = within(Duration::from_secs(5), move || {
        closer.close_and_wait(10_000)
    });
    let elapsed = started.elapsed();
    assert_eq!(outcome, Ok(CloseOutcome::Closed));
    assert!(elapsed >= Duration::from_millis(450), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(2_000), "{elapsed:?}");
    assert_eq!(controller.stats().invalidate_timeouts, 1);

    // The platform call returns later; its output is stale and its permit goes.
    let (stuck, cv) = &*encoder.unstick;
    *lock(stuck) = true;
    cv.notify_all();
    encoder.complete(1);
    assert_eq!(fakes.log.native_permits.load(Ordering::SeqCst), 0);
    assert!(fakes.out.packets().is_empty());
    queue.quit();
    fakes.assert_clean();
}

#[test]
fn reentrant_native_adapters_cannot_deadlock_the_stream() {
    within(Duration::from_secs(20), || {
        let fakes = Fakes::new();
        // Every native call answers synchronously, back into Rust.
        fakes.capture.auto_answer.store(true, Ordering::SeqCst);
        fakes.capture.frame_on_start.store(true, Ordering::SeqCst);
        fakes.factory.sync_output.store(true, Ordering::SeqCst);
        fakes.out.sync_receipts.store(true, Ordering::SeqCst);
        let clock = Arc::new(ManualClock(std::sync::atomic::AtomicU64::new(SECOND)));
        let port = StreamPort::new();
        let queue = ManualQueue::new(port.clone(), clock.clone());
        // And the queue breaks "never inline": wake runs the drain at once.
        queue.inline_wake.store(true, Ordering::SeqCst);
        let controller =
            StreamController::with_clock(fakes.parts(port, queue.clone(), plan()), clock.clone())
                .expect("valid");
        // The observer calls straight back into the controller.
        let weak = Arc::downgrade(&controller);
        *lock(&fakes.observer.reentry) = Some(Box::new(move |job| {
            if let Some(controller) = weak.upgrade() {
                if !matches!(job, ObserverJob::Stats { .. }) {
                    controller.request_stats();
                }
                if matches!(job, ObserverJob::Started { .. }) {
                    controller.request_keyframe();
                }
            }
        }));
        controller.start();
        queue.pump();
        for n in 0..20 {
            fakes.capture.emit(srgb(), 2_000 + n);
            queue.pump();
        }
        assert!(fakes.out.samples().len() >= 20);
        controller.begin_close();
        queue.pump();
        assert_eq!(controller.close_outcome(), Some(CloseOutcome::Closed));
        *lock(&fakes.observer.reentry) = None;
        fakes.assert_clean();
        fakes.assert_wire_order();
    });
}

#[test]
fn async_close_completes_on_a_real_queue() {
    let fakes = Fakes::new();
    fakes.capture.auto_answer.store(true, Ordering::SeqCst);
    fakes.factory.sync_output.store(true, Ordering::SeqCst);
    let port = StreamPort::new();
    let queue = ThreadQueue::new(port.clone());
    let controller =
        StreamController::new(fakes.parts(port, queue.clone(), plan())).expect("valid");
    controller.start();
    let capture = fakes.capture.clone();
    queue.run_on(move || {
        capture.emit(srgb(), 1_000);
    });
    queue.flush();
    let outcome = within(Duration::from_secs(5), move || block_on(controller.close()));
    assert_eq!(outcome, CloseOutcome::Closed);
    assert_eq!(fakes.observer.closed(), Some(CloseOutcome::Closed));
    queue.quit();
    fakes.assert_clean();
}

#[test]
fn dropping_a_controller_closes_the_stream() {
    let fakes = Fakes::new();
    fakes.capture.auto_answer.store(true, Ordering::SeqCst);
    let port = StreamPort::new();
    let queue = ThreadQueue::new(port.clone());
    let (tx, rx) = mpsc::channel();
    *lock(&fakes.observer.closed_tx) = Some(tx);
    let controller =
        StreamController::new(fakes.parts(port, queue.clone(), plan())).expect("valid");
    controller.start();
    queue.flush();
    drop(controller);
    let outcome = rx.recv_timeout(Duration::from_secs(5)).expect("closed");
    assert_eq!(outcome, CloseOutcome::Closed);
    assert_eq!(fakes.capture.count(Op::Stop), 1);
    queue.quit();
    fakes.assert_clean();
}

#[test]
fn blocking_entry_points_are_refused_on_executors_and_network_workers() {
    assert!(blocking_allowed());
    let on_network = std::thread::Builder::new()
        .name(NETWORK_THREAD_NAME.into())
        .spawn(blocking_allowed)
        .expect("thread")
        .join()
        .expect("join");
    assert!(!on_network);

    // Inside a drain (here: inside an encoder submit) neither is allowed.
    let s = Manual::new();
    s.start_running();
    let (tx, rx) = mpsc::channel();
    *lock(&s.encoder().on_submit) = Some(Box::new(move || {
        tx.send((blocking_allowed(), on_executor())).ok();
    }));
    s.frame(1_000);
    assert_eq!(rx.recv().expect("submit ran"), (false, true));
    *lock(&s.encoder().on_submit) = None;
    s.close();
}

/// Debug builds assert (the panic hook reports "rule 6"); the drain contains
/// the panic like any other, failing and closing the stream.
#[cfg(debug_assertions)]
#[test]
fn close_and_wait_on_an_executor_asserts_in_debug_builds() {
    let s = Manual::new();
    s.start_running();
    let controller = s.controller.clone();
    s.frame(1_000);
    *lock(&s.encoder().on_submit) = Some(Box::new(move || {
        let _ = controller.close_and_wait(10);
    }));
    s.encoder().complete(1);
    s.pump();
    s.frame(2_000);
    s.pump();
    assert_eq!(s.observer.failures(), vec![codes::OUTPUT.to_string()]);
    s.capture.answer(Op::Stop, None);
    s.pump();
    assert_eq!(s.controller.close_outcome(), Some(CloseOutcome::Closed));
}

#[test]
fn closing_from_an_unknown_start_reports_stop_unknown() {
    let s = Manual::new();
    let op = s.controller.start();
    s.pump();
    s.advance_ms(10_001);
    assert_eq!(s.observer.results(op), vec![OpOutcome::OutcomeUnknown]);
    s.controller.begin_close();
    s.pump();
    assert_eq!(
        s.controller.close_outcome(),
        Some(CloseOutcome::StopUnknown)
    );
    assert_eq!(s.controller.phase(), StreamPhase::Closed);
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        std::thread::park_timeout(Duration::from_millis(50));
    }
}

#[test]
fn a_session_whose_idr_was_lost_sends_nothing_until_a_new_idr() {
    let s = Manual::new();
    s.start_running();
    let encoder = s.encoder();
    s.frame(1_000);
    s.frame(2_000);
    assert_eq!(encoder.pending_seqs(), vec![1, 2]);
    assert!(lock(&encoder.submits)[0].force_keyframe);
    assert!(!lock(&encoder.submits)[1].force_keyframe);
    encoder.drop_output(1, DropReason::NoOutput);
    encoder.complete(2);
    s.pump();
    // The delta would open a generation without a keyframe: it is a loss.
    assert!(s.out.packets().is_empty());
    let submits = lock(&encoder.submits).clone();
    assert_eq!(submits.len(), 3);
    assert!(
        submits[2].force_keyframe,
        "the retained frame is re-sent as an IDR"
    );
    encoder.complete(3);
    s.pump();
    let packets = s.out.packets();
    assert!(packets[0].meta.config && packets[1].meta.keyframe);
    s.close();
    s.assert_clean();
    s.assert_wire_order();
}
