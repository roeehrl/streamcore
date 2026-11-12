//! The bench: one stream through a real `StreamController` on the Windows backends, with the
//! recorder collecting per-stage stamps.
//!
//! Synthetic mode opens a topmost window over the whole monitor that draws a frame-counter
//! barcode (and, with `--pace MS`, flips its colour) at known QPC times; the converter reads the
//! barcode back from every converted frame, so each submitted frame is matched to the moment its
//! content was drawn, and capture -> encoded is timed without a client. Desktop mode captures
//! the desktop as it is (WGC delivers a frame only when something changes).

use super::capture::{SDR_DESKTOP, WgcCapture};
use super::convert::{Strip, VpConverter};
use super::encoder::{MfEncoderFactory, plan_permits};
use super::queue::ExecutorThread;
use super::surfaces::Surfaces;
use super::sys::com::now_ns;
use super::sys::d3d::{Gpu, OutputDesc};
use super::sys::window::{Pace as WindowPace, Strip as WindowStrip, Synthetic};
use super::{Error, err, os_build, probe, sizes};
use crate::barcode;
use crate::cli::{Mode, Options, Pace};
use crate::model::{BenchReport, SyntheticResult};
use crate::trace::{FrameTrace, Recorder, summarize_stages};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use streamcore_video::{
    CaptureSpec, CaptureTarget, Codec, ColorMapping, ColorRequest, ConvertPath, CursorMode,
    ObserverJob, ObserverQueue, OnSourceGone, PixelFormat, RecoveryStep, SinkReceipt,
    StreamController, StreamMode, StreamParts, StreamPlan, StreamPort, VideoOut, VideoOutMeta,
};

/// `VideoOut` for the bench: stamps H5 and accepts every packet.
struct BenchOut {
    recorder: Arc<Recorder>,
    revoked: AtomicBool,
}

impl VideoOut for BenchOut {
    fn send(&self, meta: VideoOutMeta, _bytes: Vec<u8>, receipt: Arc<SinkReceipt>) {
        let h5 = now_ns();
        if !meta.config {
            self.recorder.sent(meta.header.sequence, h5);
        }
        receipt.complete(!self.revoked.load(Ordering::Acquire));
    }

    fn backpressured(&self) -> bool {
        false
    }

    fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }
}

struct Observer(Mutex<mpsc::Sender<ObserverJob>>);

impl ObserverQueue for Observer {
    fn post(&self, job: ObserverJob) {
        let _ = self.0.lock().unwrap_or_else(|p| p.into_inner()).send(job);
    }
}

const STRIP: WindowStrip = WindowStrip {
    x: 16,
    y: 16,
    width: 0,
    height: 48,
};

pub fn run(options: &Options, luid: u64, output: &OutputDesc) -> Result<BenchReport, Error> {
    let (capture_size, encode) = sizes(output, options.size);
    let bitrate = options.bitrate_for(encode.0, encode.1);
    let mut notes = Vec::new();
    let mut errors = Vec::new();

    // The permit target comes from the encoder's min_inputs_before_output (3.3).
    let probe = probe::run(luid, encode.0, encode.1, options.fps, bitrate);
    let min_inputs = probe.as_ref().ok().and_then(|p| p.min_inputs_before_output);
    if let Err(e) = &probe {
        errors.push(format!("probe: {e}"));
    }
    let permits = options.permits.unwrap_or_else(|| plan_permits(min_inputs));
    // WGC buffers: the latest slot + the permits in flight (each keeps its capture lease until
    // its output) + one for WGC to write into.
    let buffers = options.buffers.unwrap_or(permits + 3);

    let gpu = Arc::new(Gpu::create(Some(luid)).map_err(err("D3D11 device"))?);
    let surfaces = Arc::new(Surfaces::default());
    let recorder = Arc::new(Recorder::default());
    let capture = WgcCapture::new(
        gpu.clone(),
        surfaces.clone(),
        Some(recorder.clone()),
        options.timer_period,
    )?;
    let converter = VpConverter::new(
        gpu.clone(),
        surfaces.clone(),
        Some(recorder.clone()),
        usize::from(permits) + 2,
        options.fps,
    )?;
    let factory = MfEncoderFactory::new(
        gpu.clone(),
        surfaces.clone(),
        converter.fence(),
        Some(recorder.clone()),
        min_inputs,
    );

    // Synthetic source over the whole monitor, its barcode read back after convert.
    let synthetic = options.mode == Mode::Synthetic;
    let source = if synthetic {
        let cell = ((capture_size.0 as i32 - 32) / barcode::CELLS as i32).clamp(4, 40);
        let strip = WindowStrip {
            width: cell * barcode::CELLS as i32,
            ..STRIP
        };
        let dest = VpConverter::dest(capture_size, encode);
        let sx = f64::from(dest.width) / f64::from(capture_size.0);
        let sy = f64::from(dest.height) / f64::from(capture_size.1);
        converter.set_readback(
            Strip {
                y: (dest.top + (f64::from(strip.y + strip.height / 2) * sy) as u32) & !1,
                x0: f64::from(dest.left) + f64::from(strip.x) * sx,
                width: f64::from(strip.width) * sx,
            },
            encode.0,
            8,
        )?;
        Some(Synthetic::start(
            (
                output.desktop[0],
                output.desktop[1],
                capture_size.0 as i32,
                capture_size.1 as i32,
            ),
            strip,
            match options.pace {
                Pace::Composition => WindowPace::EveryComposition,
                Pace::EveryMs(ms) => WindowPace::Every(ms),
            },
        ))
    } else {
        None
    };

    let executor = ExecutorThread::spawn()?;
    if let Some(e) = &executor.mmcss_error {
        notes.push(e.clone());
    }
    let port = StreamPort::new();
    executor.bind(port.clone());
    let (jobs_tx, jobs) = mpsc::channel();
    let mapping = ColorMapping {
        allow_p3: false,
        srgb_transfer_as_bt709: false,
    };
    let min_interval_ns = if options.min_update_interval {
        1_000_000_000 / u64::from(options.fps)
    } else {
        0
    };
    let out = Arc::new(BenchOut {
        recorder: recorder.clone(),
        revoked: AtomicBool::new(false),
    });
    let parts = StreamParts {
        stream_id: 1,
        spec: CaptureSpec {
            target: CaptureTarget::Display {
                display_id: options.monitor,
            },
            mode: StreamMode::View,
            pixel_width: capture_size.0,
            pixel_height: capture_size.1,
            min_interval_ns,
            format: PixelFormat::Bgra,
            color: ColorRequest::Source,
            cursor: if options.cursor {
                CursorMode::Composited
            } else {
                CursorMode::Hidden
            },
            queue_depth: buffers,
            adapter_luid: None,
        },
        plan: StreamPlan {
            mode: StreamMode::View,
            codec: Codec::H264,
            encode_width: encode.0,
            encode_height: encode.1,
            fps: options.fps,
            bitrate_bps: bitrate,
            convert: ConvertPath::Convert,
            encoder_input: PixelFormat::Nv12,
            encoder: "Media Foundation hardware H.264".into(),
            color_out: mapping.map(SDR_DESKTOP, PixelFormat::Bgra, PixelFormat::Nv12),
            color_mapping: mapping,
            extensions: Vec::new(),
            permit_target: permits,
            recovery: vec![RecoveryStep::Idr],
            on_source_gone: OnSourceGone::Fail,
            medium_cap_bps: None,
            adapter_luid: Some(luid),
            refresh_interval_ns: None,
            pacing: streamcore_video::PacingPolicy::default(),
        },
        port,
        queue: executor.queue(),
        capture: capture.clone(),
        converter: Some(converter.clone()),
        encoders: factory.clone(),
        out: out.clone(),
        observer: Arc::new(Observer(Mutex::new(jobs_tx))),
        timings: None,
    };
    let controller = StreamController::new(parts).map_err(err("StreamController"))?;
    let started_at = now_ns();
    controller.start();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut started = false;
    while !started && Instant::now() < deadline {
        match jobs.recv_timeout(Duration::from_millis(100)) {
            Ok(ObserverJob::Started { info, .. }) => {
                notes.push(format!(
                    "started: {} (low latency {})",
                    info.name, info.low_latency
                ));
                started = true;
            }
            Ok(ObserverJob::OpResult { outcome, .. }) => notes.push(format!("start: {outcome:?}")),
            Ok(ObserverJob::Failed { code }) => {
                errors.push(format!("stream failed: {code}"));
                break;
            }
            Ok(other) => notes.push(format!("{other:?}")),
            Err(_) => {}
        }
    }
    if !started {
        errors.push("the stream did not start".into());
    }
    let measure_from = now_ns() + (options.warmup * 1e9) as u64;
    let end_at = measure_from + (options.seconds * 1e9) as u64;
    if started {
        std::thread::sleep(Duration::from_nanos(end_at.saturating_sub(now_ns())));
    }
    let stats = controller.stats();
    // Read while the capture still holds the timer period (nothing is held after close).
    let timer = super::timer_report(options.timer_period);
    let outcome = controller.close_and_wait(10_000);
    notes.push(format!("close: {outcome:?}"));
    drop(controller);
    let log = source.map(Synthetic::stop);
    executor.stop();
    converter.shutdown();
    while let Ok(job) = jobs.try_recv() {
        match job {
            ObserverJob::Failed { code } => errors.push(format!("stream failed: {code}")),
            ObserverJob::EncoderFailed { error } => errors.push(format!("encoder failed: {error}")),
            ObserverJob::LayoutChanged { layout, .. } => notes.push(format!("layout: {layout:?}")),
            ObserverJob::SourceGone { reason } => errors.push(format!("source gone: {reason:?}")),
            _ => {}
        }
    }
    // The closed stream dropped its encoders: wait until their MFTs are shut down and their
    // event threads joined, so the session counters below are final.
    if !factory.wait_sessions(Duration::from_secs(10)) {
        errors.push("encoder sessions still shutting down 10 s after close".into());
    }
    let leaked = surfaces.len();
    if leaked > 0 {
        errors.push(format!("{leaked} surfaces still leased after close"));
    }
    notes.extend(capture.notes());
    if converter.copied_inputs() > 0 {
        notes.push(format!(
            "capture frames copied before convert: {}",
            converter.copied_inputs()
        ));
    }
    let session = factory.stats();
    errors.extend(session.errors.iter().cloned());
    if session.busy > 0 || session.held > 0 {
        notes.push(format!(
            "encoder inputs held for NeedInput {}, refused busy {}",
            session.held, session.busy
        ));
    }
    notes.push(format!(
        "run started {:.1} s before measuring",
        (measure_from.saturating_sub(started_at)) as f64 / 1e9
    ));

    let frames = recorder.frames();
    let in_window = |t: &FrameTrace| t.h1.is_some_and(|h| (measure_from..=end_at).contains(&h));
    let mut measured: Vec<FrameTrace> = frames.iter().filter(|t| in_window(t)).cloned().collect();
    let seconds = (end_at - measure_from) as f64 / 1e9;
    let interval_ns = 1_000_000_000 / u64::from(options.fps);

    let synthetic_result = match log {
        Some(Ok(log)) => Some(synthetic_analysis(
            &log,
            &mut measured,
            options,
            measure_from,
            end_at,
        )),
        Some(Err(e)) => {
            errors.push(format!("synthetic source: {e}"));
            None
        }
        None => None,
    };
    let count = |f: &dyn Fn(&FrameTrace) -> bool| measured.iter().filter(|t| f(t)).count() as u64;
    let encoded_bytes: usize = measured.iter().filter_map(|t| t.bytes).sum();
    let snapshot = stats;
    let stream: BTreeMap<String, u64> = [
        ("frames_received", snapshot.frames_received),
        ("frames_replaced", snapshot.frames_replaced),
        ("submitted", snapshot.submitted),
        ("encoded", snapshot.encoded),
        ("encoder_drops", snapshot.encoder_drops),
        ("packets_sent", snapshot.packets_sent),
        ("packets_refused", snapshot.packets_refused),
        ("recoveries", snapshot.recoveries),
        ("encoders_created", snapshot.encoders_created),
        ("invalidate_timeouts", snapshot.invalidate_timeouts),
        ("create_timeouts", snapshot.create_timeouts),
        ("stale_events", snapshot.stale_events),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    Ok(BenchReport {
        os_build: os_build(),
        mode: if synthetic { "synthetic" } else { "desktop" }.into(),
        adapter: gpu.name.clone(),
        monitor: options.monitor,
        capture_size: [capture_size.0, capture_size.1],
        encode_size: [encode.0, encode.1],
        fps_target: options.fps,
        refresh_hz: output.refresh_hz,
        seconds,
        warmup_seconds: options.warmup,
        bitrate,
        permit_target: permits,
        pool_buffers: buffers,
        min_update_interval_set: min_interval_ns > 0,
        timer,
        encoder: session.setup.clone().unwrap_or_default(),
        min_inputs_before_output_probe: min_inputs,
        inputs_before_first_output: session.inputs_before_first_output,
        max_frames_in_encoder: Some(session.max_in_encoder),
        fps_captured: recorder.captured_between(measure_from, end_at) as f64 / seconds,
        fps_submitted: count(&|t| t.h3.is_some()) as f64 / seconds,
        fps_encoded: count(&|t| t.h4.is_some()) as f64 / seconds,
        fps_sent: count(&|t| t.h5.is_some()) as f64 / seconds,
        counts: recorder.counts(),
        stream,
        stages: summarize_stages(measured.iter()),
        r1_late_sends: count(&|t| t.stage("H4->H5").is_some_and(|d| d > interval_ns)),
        synthetic: synthetic_result,
        encoded_mbps: encoded_bytes as f64 * 8.0 / 1e6 / seconds,
        keyframes: count(&|t| t.keyframe),
        sps: session.sps.clone(),
        vui_normalized: session.vui_normalized,
        errors,
        notes,
    })
}

/// Matches each submitted frame to the draw time of the counter it shows first, and counts
/// counters skipped and frames repeated.
fn synthetic_analysis(
    log: &[(u32, u64)],
    measured: &mut [FrameTrace],
    options: &Options,
    from: u64,
    to: u64,
) -> SyntheticResult {
    let draw: HashMap<u32, u64> = log.iter().map(|(c, t)| (*c & 0xffff, *t)).collect();
    let mut seen = HashSet::new();
    let mut encoded = HashSet::new();
    let mut repeated = 0;
    let mut undecoded = 0;
    for t in measured.iter_mut() {
        let Some(c) = t.counter else {
            undecoded += 1;
            continue;
        };
        if seen.insert(c) {
            t.src = draw.get(&c).copied();
        } else {
            repeated += 1;
        }
        if t.h4.is_some() {
            encoded.insert(c);
        }
    }
    // Counters drawn well inside the window: a margin for the pipeline at both ends.
    let margin = 100_000_000;
    let drawn: Vec<u32> = log
        .iter()
        .filter(|(_, t)| *t >= from + margin && *t + margin <= to)
        .map(|(c, _)| *c & 0xffff)
        .collect();
    SyntheticResult {
        pace: match options.pace {
            Pace::Composition => "a new counter every DWM composition".into(),
            Pace::EveryMs(ms) => format!("a new counter and colour every {ms} ms"),
        },
        counters_drawn: drawn.len() as u64,
        counters_encoded: drawn.iter().filter(|c| encoded.contains(*c)).count() as u64,
        counters_skipped: drawn.iter().filter(|c| !seen.contains(*c)).count() as u64,
        repeated,
        undecoded,
    }
}

pub fn print(r: &BenchReport) {
    println!("OS build {}  adapter {}", r.os_build, r.adapter);
    println!(
        "{} mode: capture {}x{} -> encode {}x{} @ {} fps target, refresh {:?} Hz, {:.1} s measured after {} s warm-up",
        r.mode,
        r.capture_size[0],
        r.capture_size[1],
        r.encode_size[0],
        r.encode_size[1],
        r.fps_target,
        r.refresh_hz,
        r.seconds,
        r.warmup_seconds
    );
    println!(
        "permits {}  WGC buffers {}  MinUpdateInterval {}  timer {:?}  bitrate {}  min_inputs_before_output (probe) {:?}  inputs before first output {:?}  max in encoder {:?}",
        r.permit_target,
        r.pool_buffers,
        r.min_update_interval_set,
        r.timer,
        r.bitrate,
        r.min_inputs_before_output_probe,
        r.inputs_before_first_output,
        r.max_frames_in_encoder
    );
    let e = &r.encoder;
    println!(
        "encoder {} ({}), low latency confirmed {}, D3D11-aware {}",
        e.name,
        if e.hardware { "hardware" } else { "software" },
        e.low_latency_confirmed,
        e.d3d11_aware
    );
    println!(
        "fps captured {:.2}  submitted {:.2}  encoded {:.2}  sent {:.2}",
        r.fps_captured, r.fps_submitted, r.fps_encoded, r.fps_sent
    );
    println!("recorder {:?}", r.counts);
    println!("stream {:?}", r.stream);
    println!(
        "{:9} {:>6} {:>8} {:>8} {:>8} {:>8}   stage",
        "row", "n", "p50 ms", "p95 ms", "p99 ms", "max ms"
    );
    for (name, desc) in crate::trace::STAGES {
        if let Some(s) = r.stages.get(*name) {
            println!(
                "{:9} {:>6} {:>8.3} {:>8.3} {:>8.3} {:>8.3}   {}",
                name, s.n, s.p50_ms, s.p95_ms, s.p99_ms, s.max_ms, desc
            );
        }
    }
    println!(
        "R1: sends later than one frame interval after the access unit: {}",
        r.r1_late_sends
    );
    if let Some(s) = &r.synthetic {
        println!("synthetic: {s:?}");
    }
    println!(
        "encoded {:.2} Mbit/s, {} keyframes",
        r.encoded_mbps, r.keyframes
    );
    if let Some(s) = &r.sps {
        println!(
            "SPS as written: profile {} level {} {}x{} zero-reorder {} VUI {:?}; normalised: {}",
            s.profile_idc,
            s.level_idc,
            s.width,
            s.height,
            s.declares_zero_reorder(),
            s.vui,
            r.vui_normalized
        );
    }
    for n in &r.notes {
        println!("note: {n}");
    }
    for e in r.errors.iter().take(20) {
        println!("error: {e}");
    }
}
