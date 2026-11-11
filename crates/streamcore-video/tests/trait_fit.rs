//! Trait-fit changes from the Windows backend (P5): the capture lease ends at
//! convert, the capture calls' thread guarantee, capture-side skips, region
//! crops through `ConvertJob::source`, and the stage times forwarded to
//! `VideoOut`.

mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;
use streamcore_video::*;
use support::*;

fn convert_plan(fakes: &Fakes, parts: &mut StreamParts) {
    parts.plan.convert = ConvertPath::Convert;
    parts.plan.encoder_input = PixelFormat::Nv12;
    parts.converter = Some(fakes.converter.clone());
}

#[test]
fn on_the_convert_path_the_capture_surface_is_not_pinned_until_the_output() {
    let s = Manual::with(convert_plan);
    s.start_running();
    let first = s.frame(1_000);
    assert_eq!(s.encoder().pending_seqs(), vec![1]);
    // The next capture replaces the latest slot. The first image's only other
    // hold was the in-flight frame, which ended at convert.
    let second = s.frame(2_000);
    let live = lock(&s.capture.live).clone();
    assert!(
        !live.contains(&first),
        "the converted frame still pins its capture"
    );
    assert!(live.contains(&second), "the latest slot keeps its image");
    assert_eq!(s.encoder().pending_seqs(), vec![1, 2]);
    s.encoder().complete_all();
    s.pump();
    assert_eq!(s.out.samples().len(), 2);
    s.close();
    s.assert_clean();
}

#[test]
fn on_the_identity_path_the_encoder_keeps_its_capture_surface_until_the_output() {
    let s = Manual::new();
    s.start_running();
    let first = s.frame(1_000);
    s.frame(2_000);
    assert!(
        lock(&s.capture.live).contains(&first),
        "the encoder reads this surface"
    );
    s.encoder().complete(1);
    s.pump();
    assert!(!lock(&s.capture.live).contains(&first));
    s.encoder().complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn capture_calls_run_on_the_queue_thread_never_in_an_inline_drain() {
    let fakes = Fakes::new();
    fakes.capture.auto_answer.store(true, Ordering::SeqCst);
    fakes
        .capture
        .region_in_surface
        .store(false, Ordering::SeqCst);
    let port = StreamPort::new();
    let queue = ThreadQueue::new(port.clone());
    let mut parts = fakes.parts(port, queue.clone(), plan());
    parts.spec.target = CaptureTarget::Region {
        display_id: 1,
        rect: NormalizedRect {
            x: 0.0,
            y: 0.0,
            width: 0.5,
            height: 0.5,
        },
    };
    convert_plan(&fakes, &mut parts);
    let controller = StreamController::new(parts).expect("valid");
    controller.start();
    queue.flush();
    queue.flush();
    assert_eq!(controller.phase(), StreamPhase::Running);
    // Frames from the queue (inline drains) and from a foreign thread.
    let capture = fakes.capture.clone();
    queue.run_on(move || {
        capture.emit(srgb(), 1_000);
    });
    queue.flush();
    let capture = fakes.capture.clone();
    std::thread::spawn(move || {
        capture.emit(srgb(), 2_000);
    })
    .join()
    .expect("frame thread");
    queue.flush();
    controller.update(StreamUpdate {
        rate: None,
        capture: CaptureUpdate {
            min_interval_ns: Some(SECOND / 30),
            region: None,
            cursor: None,
        },
        encode_width: None,
        encode_height: None,
    });
    queue.flush();
    queue.flush();
    fakes.factory.last().complete_all();
    let closer = controller.clone();
    let outcome = within(Duration::from_secs(5), move || closer.close_and_wait(5_000));
    assert_eq!(outcome, Ok(CloseOutcome::Closed));
    let threads = lock(&fakes.capture.op_threads).clone();
    // caps, open, start, update, stop.
    assert_eq!(threads.len(), 5, "{threads:?}");
    assert!(threads.iter().all(|t| *t == queue.thread));
    queue.quit();
    fakes.assert_clean();
}

#[test]
fn capture_side_skips_are_counted_and_late_ones_are_ignored() {
    let s = Manual::new();
    s.start_running();
    let events = lock(&s.capture.events).clone().expect("open");
    events.skipped(CAPTURE, None, 3);
    events.skipped(CAPTURE, None, 2);
    s.frame(1_000);
    assert_eq!(s.controller.stats().capture_skipped, 5);
    s.encoder().complete_all();
    s.pump();
    s.close();
    events.skipped(CAPTURE, None, 7);
    assert_eq!(s.controller.stats().capture_skipped, 5);
    s.assert_clean();
}

fn region_stream(in_surface: bool, convert: bool) -> Manual {
    Manual::with(|fakes, parts| {
        fakes
            .capture
            .region_in_surface
            .store(in_surface, Ordering::SeqCst);
        parts.spec.target = CaptureTarget::Region {
            display_id: 1,
            rect: NormalizedRect {
                x: 0.25,
                y: 0.25,
                width: 0.5,
                height: 0.5,
            },
        };
        if convert {
            convert_plan(fakes, parts);
        }
    })
}

#[test]
fn a_whole_display_capture_is_cropped_to_the_region_and_follows_a_live_move() {
    let s = region_stream(false, true);
    s.start_running();
    s.frame(1_000);
    let op = s.controller.update(StreamUpdate {
        rate: None,
        capture: CaptureUpdate {
            min_interval_ns: None,
            region: Some(NormalizedRect {
                x: 0.5,
                y: 0.0,
                width: 0.5,
                height: 0.25,
            }),
            cursor: None,
        },
        encode_width: None,
        encode_height: None,
    });
    s.pump();
    s.capture.answer(Op::Update, None);
    s.pump();
    assert_eq!(s.observer.results(op), vec![OpOutcome::Completed]);
    s.frame(2_000);
    let sources: Vec<_> = lock(&s.converter.jobs)
        .iter()
        .map(|(_, j)| j.source)
        .collect();
    assert_eq!(
        sources,
        vec![
            Some(Rect {
                x: 16,
                y: 16,
                width: 32,
                height: 32
            }),
            Some(Rect {
                x: 32,
                y: 0,
                width: 32,
                height: 16
            }),
        ]
    );
    s.encoder().complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn a_region_the_capture_already_cropped_is_converted_whole() {
    let s = region_stream(true, true);
    s.start_running();
    s.frame(1_000);
    assert_eq!(lock(&s.converter.jobs)[0].1.source, None);
    s.encoder().complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}

#[test]
fn a_region_nothing_can_crop_is_refused_before_anything_native_starts() {
    let s = region_stream(false, false);
    let op = s.controller.start();
    s.pump();
    assert_eq!(
        s.observer.results(op),
        vec![OpOutcome::Rejected {
            code: codes::CAPTURE.into()
        }]
    );
    assert_eq!(s.factory.count(), 0);
    assert_eq!(s.capture.opens.load(Ordering::SeqCst), 0);
    s.close();
    s.assert_clean();
}

#[test]
fn the_send_hop_carries_the_frames_stage_times() {
    let s = Manual::new();
    s.start_running();
    let mut info = s.capture.surface(srgb(), 5_000);
    info.source_present_ns = Some(4_000);
    s.capture.emit_info(info);
    s.pump();
    s.encoder().complete_all();
    s.pump();
    let packets = s.out.packets();
    assert_eq!(packets.len(), 2, "config and keyframe");
    for packet in packets {
        let timing = packet.meta.timing;
        assert_eq!(timing.source_present_ns, Some(4_000));
        assert_eq!(timing.capture_ns, 5_000);
        // The fake encoder reports H3 and H4 relative to the submitted H0.
        assert_eq!(timing.encode.input_ns, Some(4_010));
        assert_eq!(timing.encode.output_ns, Some(4_020));
    }
    s.close();
    s.assert_clean();
}
