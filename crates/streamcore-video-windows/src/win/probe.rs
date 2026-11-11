//! Encoder probe (design doc 3.3, 4.2): creates the hardware H.264 MFT on the adapter, feeds it
//! one frame at a time and waits after each for output, so `min_inputs_before_output` (which sets
//! the permit target) is the number of inputs it took to get the first access unit; then times
//! `ProcessInput` -> `ProcessOutput` with one frame in flight. Runs on its own thread with blocking
//! waits: it is not a stream executor.

use super::sys::com::{Apartment, now_ns};
use super::sys::d3d::Gpu;
use super::sys::mf::{Encoder, MfEvent, Params, enumerate_h264};
use super::{Error, err, os_build};
use crate::clock::ns_to_hundred_ns;
use crate::h264::annexb::{NAL_SPS, find};
use crate::h264::sps;
use crate::mft::permit_target;
use crate::model::ProbeReport;
use crate::stats::summarize_ns;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, ID3D11Texture2D};

const MAX_INPUTS: u64 = 8;
const TIMED_FRAMES: usize = 120;

fn test_frame(gpu: &Gpu, w: u32, h: u32, shade: u32) -> Result<ID3D11Texture2D, Error> {
    let mut data = vec![128u8; (w * h * 3 / 2) as usize];
    for y in 0..h {
        for x in 0..w {
            data[(y * w + x) as usize] = (16 + (x + y + shade * 37) % 200) as u8;
        }
    }
    gpu.create_nv12(w, h, D3D11_BIND_RENDER_TARGET, Some(&data))
        .map_err(err("NV12 test frame"))
}

/// Waits for the next event, up to `timeout`.
fn next(rx: &Receiver<MfEvent>, timeout: Duration) -> Option<MfEvent> {
    rx.recv_timeout(timeout).ok()
}

struct Feeder<'a> {
    encoder: &'a Encoder,
    frames: &'a [ID3D11Texture2D],
    credits: u32,
    fed: u64,
    duration: i64,
}

impl Feeder<'_> {
    /// Waits for a credit and feeds the next frame; returns `ProcessInput`'s return time.
    fn feed(&mut self, rx: &Receiver<MfEvent>, report: &mut ProbeReport) -> Option<u64> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.credits == 0 {
            match next(rx, deadline.saturating_duration_since(Instant::now()))? {
                MfEvent::NeedInput => self.credits += 1,
                MfEvent::Failed(f) => report.errors.push(f.message),
                _ => {}
            }
        }
        let frame = &self.frames[(self.fed % self.frames.len() as u64) as usize];
        let sample = Encoder::sample(frame, self.fed as i64 * self.duration, self.duration).ok()?;
        if self.fed == 0 {
            let _ = self.encoder.force_keyframe();
        }
        if let Err(e) = self.encoder.process_input(&sample) {
            report.errors.push(format!("ProcessInput: {e}"));
            return None;
        }
        self.credits -= 1;
        self.fed += 1;
        Some(now_ns())
    }

    /// Waits up to `timeout` for an output, counting credits that arrive meanwhile.
    fn output(
        &mut self,
        rx: &Receiver<MfEvent>,
        timeout: Duration,
        report: &mut ProbeReport,
    ) -> Option<(u64, Vec<u8>)> {
        let deadline = Instant::now() + timeout;
        loop {
            match next(rx, deadline.saturating_duration_since(Instant::now()))? {
                MfEvent::NeedInput => self.credits += 1,
                MfEvent::Output(o) => return Some((o.h4_ns, o.bytes)),
                MfEvent::Failed(f) => report.errors.push(f.message),
                _ => {}
            }
        }
    }
}

pub fn run(
    luid: u64,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u64,
) -> Result<ProbeReport, Error> {
    let _apartment = Apartment::mta().map_err(err("CoInitializeEx"))?;
    let gpu = Gpu::create(Some(luid)).map_err(err("D3D11 device"))?;
    let candidates = enumerate_h264(true, Some(luid)).map_err(err("MFTEnum2"))?;
    let candidate = candidates
        .first()
        .ok_or("no hardware H.264 encoder MFT on this adapter")?;
    let params = Params {
        width,
        height,
        fps,
        bitrate: u32::try_from(bitrate).unwrap_or(u32::MAX),
    };
    let encoder = Encoder::create(candidate, &gpu, &params).map_err(err("create encoder"))?;
    let mut report = ProbeReport {
        os_build: os_build(),
        adapter: gpu.name.clone(),
        encoder: encoder.setup.clone(),
        ..ProbeReport::default()
    };
    let pump = encoder.event_pump();
    let (tx, rx) = channel();
    let events = std::thread::Builder::new()
        .name("mft-events".into())
        .spawn(move || {
            let _apartment = Apartment::mta();
            loop {
                let ev = pump.next();
                let end = matches!(ev, MfEvent::Shutdown | MfEvent::Failed(_));
                if tx.send(ev).is_err() || end {
                    break;
                }
            }
        })
        .map_err(err("spawn event thread"))?;
    let frames = (0..3)
        .map(|i| test_frame(&gpu, width, height, i))
        .collect::<Result<Vec<_>, _>>()?;
    let mut feeder = Feeder {
        encoder: &encoder,
        frames: &frames,
        credits: 0,
        fed: 0,
        duration: ns_to_hundred_ns(1_000_000_000 / u64::from(fps.max(1))),
    };
    // Patient: one input, then wait for output before the next.
    let started = now_ns();
    while feeder.fed < MAX_INPUTS {
        if feeder.feed(&rx, &mut report).is_none() {
            break;
        }
        if let Some((h4, bytes)) = feeder.output(&rx, Duration::from_millis(300), &mut report) {
            report.min_inputs_before_output = Some(feeder.fed);
            report.first_output_ms = Some((h4.saturating_sub(started)) as f64 / 1e6);
            report.sps = find(&bytes, NAL_SPS).and_then(sps::parse);
            break;
        }
    }
    // One in flight: ProcessInput -> ProcessOutput.
    let mut encode = Vec::new();
    if report.min_inputs_before_output == Some(1) {
        for _ in 0..TIMED_FRAMES {
            let Some(h3) = feeder.feed(&rx, &mut report) else {
                break;
            };
            match feeder.output(&rx, Duration::from_millis(500), &mut report) {
                Some((h4, _)) => encode.push(h4.saturating_sub(h3)),
                None => break,
            }
        }
    }
    report.encode_ms = (!encode.is_empty()).then(|| summarize_ns(&encode));
    report.permit_target = permit_target(report.min_inputs_before_output);
    encoder.shutdown();
    let _ = events.join();
    Ok(report)
}

pub fn print(r: &ProbeReport) {
    println!("OS build {}  adapter {}", r.os_build, r.adapter);
    println!("timer {:?}", r.timer);
    let e = &r.encoder;
    println!(
        "encoder {} ({}), async {}, D3D11-aware {}, low latency confirmed {}, provides samples {}",
        e.name,
        if e.hardware { "hardware" } else { "software" },
        e.asynchronous,
        e.d3d11_aware,
        e.low_latency_confirmed,
        e.provides_output_samples
    );
    for s in &e.settings {
        println!(
            "  {:30} requested {:>10}  {:10} readback {}",
            s.name,
            s.requested,
            s.status,
            s.readback.as_deref().unwrap_or("-")
        );
    }
    println!(
        "min_inputs_before_output {:?} -> permit target {}  first output after {:?} ms",
        r.min_inputs_before_output, r.permit_target, r.first_output_ms
    );
    if let Some(s) = &r.encode_ms {
        println!(
            "encode (one in flight) n {}  p50 {:.3}  p95 {:.3}  p99 {:.3}  max {:.3} ms",
            s.n, s.p50_ms, s.p95_ms, s.p99_ms, s.max_ms
        );
    }
    if let Some(s) = &r.sps {
        println!(
            "SPS profile {} level {} {}x{} zero-reorder declared {} VUI {:?}",
            s.profile_idc,
            s.level_idc,
            s.width,
            s.height,
            s.declares_zero_reorder(),
            s.vui
        );
    }
    for n in &e.notes {
        println!("note: {n}");
    }
    for e in r.errors.iter().take(20) {
        println!("error: {e}");
    }
}
