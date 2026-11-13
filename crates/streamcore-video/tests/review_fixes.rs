//! Cases from the Windows (P5) review: RGB sources on YUV encoders, adapter
//! selection, the `Busy` wake, a hung invalidate, Annex-B outputs, a slow
//! `create`, and layout changes.

mod support;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use streamcore_video::*;
use support::*;

const BT709_LIMITED_SRGB: ColorTags = ColorTags {
    primaries: ColorPrimaries::Bt709,
    transfer: TransferFunction::Srgb,
    matrix: YCbCrMatrix::Bt709,
    full_range: false,
};

#[test]
fn colour_mapping_describes_the_encoders_yuv_not_the_sources_rgb() {
    let mapping = ColorMapping {
        allow_p3: true,
        srgb_transfer_as_bt709: false,
    };
    // An RGB source honestly tagged as RGB.
    let rgb = ColorTags {
        matrix: YCbCrMatrix::Identity,
        full_range: true,
        ..BT709_LIMITED_SRGB
    };
    assert_eq!(
        mapping.map(rgb, PixelFormat::Bgra, PixelFormat::Nv12),
        BT709_LIMITED_SRGB
    );
    // Untagged or unspecified matrix, or a YUV matrix attached to RGB pixels.
    for matrix in [
        YCbCrMatrix::Unspecified,
        YCbCrMatrix::Bt709,
        YCbCrMatrix::Bt2020,
    ] {
        let source = ColorTags { matrix, ..rgb };
        assert_eq!(
            mapping.map(source, PixelFormat::Bgra, PixelFormat::P010),
            BT709_LIMITED_SRGB
        );
    }
    // A YUV source keeps its matrix and range; an untagged one gets BT.709.
    let full_601 = ColorTags {
        matrix: YCbCrMatrix::Bt601,
        full_range: true,
        ..BT709_LIMITED_SRGB
    };
    assert_eq!(
        mapping.map(full_601, PixelFormat::Nv12, PixelFormat::Nv12),
        full_601
    );
    let untagged = ColorTags {
        matrix: YCbCrMatrix::Unspecified,
        ..BT709_LIMITED_SRGB
    };
    assert_eq!(
        mapping.map(untagged, PixelFormat::Nv12, PixelFormat::Nv12),
        BT709_LIMITED_SRGB
    );
    // An RGB encoder input is identity, full range.
    let out = mapping.map(rgb, PixelFormat::Bgra, PixelFormat::Bgra);
    assert_eq!((out.matrix, out.full_range), (YCbCrMatrix::Identity, true));
}

#[test]
fn a_bgra_source_is_converted_to_bt709_limited_nv12_with_one_encoder_session() {
    let s = Manual::with(|fakes, parts| {
        parts.spec.format = PixelFormat::Bgra;
        parts.plan.convert = ConvertPath::Convert;
        parts.plan.encoder_input = PixelFormat::Nv12;
        parts.plan.color_out = BT709_LIMITED_SRGB;
        parts.converter = Some(fakes.converter.clone());
    });
    s.start_running();
    let rgb = ColorTags {
        matrix: YCbCrMatrix::Identity,
        full_range: true,
        ..BT709_LIMITED_SRGB
    };
    for n in 1..=3 {
        s.capture.emit_as(PixelFormat::Bgra, rgb, n * 1_000);
        s.pump();
        s.encoder().complete_all();
        s.pump();
    }
    let jobs = lock(&s.converter.jobs).clone();
    assert_eq!(jobs.len(), 3);
    for (_, job) in &jobs {
        assert_eq!(job.format, PixelFormat::Nv12);
        assert_eq!(
            job.color, BT709_LIMITED_SRGB,
            "the converter writes BT.709 limited YUV"
        );
    }
    assert_eq!(
        s.factory.count(),
        1,
        "no re-create: the mapped tags match the session"
    );
    let spec = &s.encoder().spec;
    assert_eq!(
        (spec.input_format, spec.color),
        (PixelFormat::Nv12, BT709_LIMITED_SRGB)
    );
    assert_eq!(s.out.samples().len(), 3);
    s.close();
    s.assert_clean();
}

#[test]
fn the_plans_adapter_reaches_capture_and_encoder() {
    let luid = (7u64 << 32) | 42;
    let s = Manual::with(|_, parts| parts.plan.adapter_luid = Some(luid));
    s.start_running();
    let opened = lock(&s.capture.opened).clone().expect("opened");
    assert_eq!(opened.adapter_luid, Some(luid));
    assert_eq!(s.encoder().spec.adapter, Some(luid));
    s.close();
    s.assert_clean();
}

#[test]
fn a_busy_encoder_that_frees_later_gets_the_retained_frame_with_no_new_capture() {
    let s = Manual::new();
    s.start_running();
    let encoder = s.encoder();
    encoder.busy_next.store(true, Ordering::SeqCst);
    s.frame(1_000);
    // A still screen: nothing else arrives.
    s.advance_ms(2_000);
    assert!(encoder.pending_seqs().is_empty());
    encoder.ready();
    s.pump();
    assert_eq!(encoder.pending_seqs(), vec![1]);
    assert!(lock(&encoder.submits)[0].force_keyframe);
    encoder.complete(1);
    s.pump();
    assert_eq!(s.out.samples().len(), 1);
    // A late wake from a replaced session changes nothing.
    s.close();
    s.assert_clean();
}

#[test]
fn a_hung_invalidate_fails_the_session_without_stalling_admission() {
    let s = Manual::new();
    *lock(&s.factory.invalidate_mode) = InvalidateMode::Stuck;
    s.start_running();
    let first = s.encoder();
    for n in 1..=3 {
        s.frame(n * 1_000);
    }
    assert_eq!(
        first.pending_seqs(),
        vec![1, 2, 3],
        "every permit is in the hung session"
    );
    // A colour change re-creates the session; its flush hangs.
    *lock(&s.factory.invalidate_mode) = InvalidateMode::Quick;
    s.capture.emit(p3(), 4_000);
    let started = Instant::now();
    s.pump();
    assert!(started.elapsed() >= Duration::from_millis(450));
    let second = s.encoder();
    assert_ne!(second.index, first.index);
    assert_eq!(
        second.pending_seqs().len(),
        1,
        "admission is not stalled by the hung session"
    );
    assert!(s.observer.jobs().iter().any(|job| matches!(
        job,
        ObserverJob::EncoderFailed {
            error: MediaError::EncoderFailed { recoverable: true }
        }
    )));
    assert_eq!(s.controller.stats().invalidate_timeouts, 1);
    assert!(s.observer.failures().is_empty());

    // The hung flush finally returns; its outputs are stale.
    let (stuck, cv) = &*first.unstick;
    *lock(stuck) = true;
    cv.notify_all();
    first.complete_all();
    s.pump();
    second.complete_all();
    s.pump();
    assert_eq!(
        s.out.samples().len(),
        1,
        "only the new session's output is sent"
    );
    s.close();
    assert_eq!(s.capture.double_release.load(Ordering::SeqCst), 0);
    assert_eq!(s.capture.leases_outstanding(), 0);
    assert_eq!(s.log.after_close.load(Ordering::SeqCst), 0);
    s.assert_wire_order();
}

#[test]
fn an_annexb_output_fails_the_stream_and_sends_nothing() {
    let s = Manual::new();
    s.start_running();
    s.frame(1_000);
    s.encoder().complete_as(1, BitstreamForm::AnnexB);
    s.pump();
    assert!(s.out.packets().is_empty());
    assert_eq!(s.observer.failures(), vec![codes::ENCODE.to_string()]);
    s.close();
    s.assert_clean();
}

#[test]
fn annexb_converts_to_four_byte_length_prefixes() {
    use streamcore_video::bitstream::annexb_to_length_prefixed;
    let annexb = vec![
        0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5, 6,
    ];
    assert_eq!(
        annexb_to_length_prefixed(annexb),
        Some(vec![
            0, 0, 0, 3, 0x67, 1, 2, 0, 0, 0, 2, 0x68, 3, 0, 0, 0, 4, 0x65, 4, 5, 6
        ])
    );
    assert_eq!(annexb_to_length_prefixed(vec![0x65, 1, 2]), None);
    assert_eq!(
        annexb_to_length_prefixed(vec![0, 0, 1, 0, 0, 1, 0x65]),
        None
    );
}

#[test]
fn a_slow_create_is_bounded_and_the_late_session_is_invalidated() {
    let s = Manual::with(|fakes, parts| {
        *lock(&fakes.factory.create_delay) = Duration::from_millis(600);
        parts.timings = Some(StreamTimings {
            create_bound_ns: 150 * MS,
            ..StreamTimings::default()
        });
    });
    let start = s.controller.start();
    let began = Instant::now();
    s.pump();
    assert!(
        began.elapsed() < Duration::from_millis(500),
        "the executor waited only the bound"
    );
    assert_eq!(
        s.observer.results(start),
        vec![OpOutcome::Rejected {
            code: codes::ENCODER.into()
        }]
    );
    assert_eq!(
        s.capture.opens.load(Ordering::SeqCst),
        0,
        "no capture without an encoder"
    );
    assert_eq!(s.controller.stats().create_timeouts, 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    while s.factory.count() == 0 || s.factory.last().invalidated.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < deadline,
            "the late session was never invalidated"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    s.close();
    // The abandoned session is invalidated on the helper thread, by design; the
    // executor itself made no outbound call off a drain.
    assert_eq!(s.log.off_executor.load(Ordering::SeqCst), 1);
    assert_eq!(s.factory.create_on_executor.load(Ordering::SeqCst), 0);
    assert_eq!(s.log.after_close.load(Ordering::SeqCst), 0);
}

#[test]
fn a_layout_change_is_reported_with_an_epoch() {
    let s = Manual::new();
    s.start_running();
    let events = lock(&s.capture.events).clone().expect("open");
    let layout = DisplayLayout {
        display_id: 1,
        pixel_width: 2560,
        pixel_height: 1440,
        scale: 2.0,
        origin_x: 0,
        origin_y: 0,
        rotation: DisplayRotation::Identity,
    };
    events.layout_changed(CAPTURE + 1, layout.clone());
    events.layout_changed(CAPTURE, layout.clone());
    events.layout_changed(CAPTURE, layout.clone());
    s.pump();
    let epochs: Vec<_> = s
        .observer
        .jobs()
        .into_iter()
        .filter_map(|job| match job {
            ObserverJob::LayoutChanged { epoch, layout: l } => {
                assert_eq!(l, layout);
                Some(epoch)
            }
            _ => None,
        })
        .collect();
    assert_eq!(epochs, vec![1, 2], "another capture's change is stale");
    s.close();
    s.assert_clean();
}

/// A single display turned 90° reports a new layout (size swapped, its
/// rotation): a new epoch for the consumer to re-plan with, never a gone
/// source.
#[test]
fn a_rotation_is_a_layout_change_with_a_new_epoch() {
    let s = Manual::new();
    s.start_running();
    let events = lock(&s.capture.events).clone().expect("open");
    let upright = DisplayLayout {
        display_id: 1,
        pixel_width: 2560,
        pixel_height: 1440,
        scale: 2.0,
        origin_x: 0,
        origin_y: 0,
        rotation: DisplayRotation::Identity,
    };
    let turned = DisplayLayout {
        pixel_width: 1440,
        pixel_height: 2560,
        rotation: DisplayRotation::Rotate90,
        ..upright.clone()
    };
    events.layout_changed(CAPTURE, upright);
    events.layout_changed(CAPTURE, turned.clone());
    s.pump();
    let changes: Vec<_> = s
        .observer
        .jobs()
        .into_iter()
        .filter_map(|j| match j {
            ObserverJob::LayoutChanged { epoch, layout } => Some((epoch, layout)),
            _ => None,
        })
        .collect();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[1], (2, turned));
    assert!(s.observer.failures().is_empty());
    assert!(
        !s.observer
            .jobs()
            .iter()
            .any(|j| matches!(j, ObserverJob::SourceGone { .. }))
    );
    s.close();
    s.fakes.assert_clean();
}
