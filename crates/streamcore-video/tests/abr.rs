//! Rate adaptation (6.6) on a simulated throttled link: a real stream (its
//! executor, pacer, sink and controller) over a transport that serialises at a
//! fixed capacity through a kernel-like send buffer, on the manual clock. The
//! fake encoder sizes its outputs by the bitrate it was given, so the link
//! sees what rate adaptation chose. Also: recovery when the sink leaves
//! backpressure (follow-up to 6.5) and the flag's off state.

mod support;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use streamcore_media::media::{ExtendedVideoPacket, FrameVersion};
use streamcore_transport::channel::LinkStats;
use streamcore_video::*;
use support::*;

const KIB: usize = 1024;

/// One enqueued message.
struct Message {
    bytes: usize,
    /// (generation, sequence) of a video payload.
    frame: Option<(u64, u64)>,
    bulk: bool,
    ticket: Option<Arc<SendTicket>>,
}

#[derive(Default)]
struct LinkState {
    /// Enqueued, not yet in the kernel (the multiplexer's queue).
    app: VecDeque<Message>,
    /// Bulk messages waiting for the bucket.
    bulk: VecDeque<Message>,
    /// In the kernel send buffer: (message, bytes left to serialise).
    kernel: VecDeque<(Message, usize)>,
    kernel_bytes: usize,
    /// Serialised, arriving at the given µs.
    flight: VecDeque<(u64, Message)>,
    /// (generation, sequence, receive µs) of delivered video, not yet reported.
    delivered: Vec<(u64, u64, u64)>,
    /// (written µs, receive µs) of every video frame.
    video_times: Vec<(u64, u64)>,
    written_at: std::collections::HashMap<(u64, u64), u64>,
    bulk_sent: u64,
    bulk_delivered: u64,
    /// Bulk bucket refill rate in bytes/s (0: unlimited), and its tokens.
    bulk_rate: u64,
    bulk_tokens: f64,
    rate_inputs: Vec<(u64, u64)>,
}

/// A link of `capacity_bps` behind a `kernel_bytes` send buffer, `one_way_us`
/// of propagation. Messages enter the kernel buffer while it has room (that is
/// "written"), serialise at the capacity and are consumed on arrival.
struct SimLink {
    clock: Arc<ManualClock>,
    capacity_bps: u64,
    kernel_limit: usize,
    one_way_us: u64,
    bucket: AtomicBool,
    state: Mutex<LinkState>,
    last_us: AtomicU64,
}

impl SimLink {
    fn new(clock: Arc<ManualClock>, capacity_bps: u64, kernel_limit: usize) -> Arc<Self> {
        Arc::new(Self {
            clock,
            capacity_bps,
            kernel_limit,
            one_way_us: 2_000,
            bucket: AtomicBool::new(false),
            state: Mutex::new(LinkState::default()),
            last_us: AtomicU64::new(0),
        })
    }

    fn now_us(&self) -> u64 {
        self.clock.now_ns() / 1_000
    }

    /// Queue `count` bulk messages of `bytes` (a file transfer).
    fn bulk(&self, count: usize, bytes: usize) {
        let mut state = lock(&self.state);
        for _ in 0..count {
            state.bulk.push_back(Message {
                bytes,
                frame: None,
                bulk: true,
                ticket: None,
            });
        }
    }

    /// Advance the link to now; tickets are completed outside the lock.
    fn step(&self) {
        let now = self.now_us();
        let last = self.last_us.swap(now, Ordering::SeqCst);
        let elapsed = if last == 0 { 0 } else { now - last };
        let mut written = Vec::new();
        let mut consumed = Vec::new();
        {
            let mut guard = lock(&self.state);
            let state = &mut *guard;
            // Serialise from the kernel buffer at the link's capacity.
            let mut budget = (self.capacity_bps as u128 * elapsed as u128 / 8_000_000) as usize;
            while budget > 0 {
                let Some((_, left)) = state.kernel.front_mut() else {
                    break;
                };
                let take = (*left).min(budget);
                *left -= take;
                budget -= take;
                state.kernel_bytes -= take;
                if *left == 0 {
                    let (message, _) = state.kernel.pop_front().unwrap();
                    state.flight.push_back((now + self.one_way_us, message));
                }
            }
            // Bulk tokens refill at the bucket's rate.
            if state.bulk_rate != 0 {
                state.bulk_tokens = (state.bulk_tokens
                    + state.bulk_rate as f64 * elapsed as f64 / 1e6)
                    .min(64.0 * KIB as f64);
            }
            // Fill the kernel buffer: video first (lane priority), then bulk in
            // 16 KiB fragments while the bucket allows.
            while state.kernel_bytes < self.kernel_limit {
                if let Some(message) = state.app.pop_front() {
                    state.kernel_bytes += message.bytes;
                    if let Some(frame) = message.frame {
                        state.written_at.insert(frame, now);
                    }
                    if let Some(ticket) = &message.ticket {
                        written.push(ticket.clone());
                    }
                    let bytes = message.bytes;
                    state.kernel.push_back((message, bytes));
                    continue;
                }
                let bucket_open = state.bulk_rate == 0 || state.bulk_tokens >= 0.0;
                if !bucket_open {
                    break;
                }
                let Some(front) = state.bulk.front_mut() else {
                    break;
                };
                let chunk = front.bytes.min(16 * KIB);
                front.bytes -= chunk;
                if front.bytes == 0 {
                    state.bulk.pop_front();
                }
                if state.bulk_rate != 0 {
                    state.bulk_tokens -= chunk as f64;
                }
                state.bulk_sent += chunk as u64;
                state.kernel_bytes += chunk;
                state.kernel.push_back((
                    Message {
                        bytes: chunk,
                        frame: None,
                        bulk: true,
                        ticket: None,
                    },
                    chunk,
                ));
            }
            // Arrivals: the client consumes at once.
            while state.flight.front().is_some_and(|(at, _)| *at <= now) {
                let (at, mut message) = state.flight.pop_front().unwrap();
                if message.bulk {
                    state.bulk_delivered += message.bytes as u64;
                }
                if let Some((generation, sequence)) = message.frame {
                    state.delivered.push((generation, sequence, at));
                    let written_at = state
                        .written_at
                        .remove(&(generation, sequence))
                        .unwrap_or(at);
                    state.video_times.push((written_at, at));
                }
                if let Some(ticket) = message.ticket.take() {
                    consumed.push(ticket);
                }
            }
        }
        for ticket in written {
            ticket.written(Some(now));
        }
        for ticket in consumed {
            ticket.consumed();
        }
    }

    fn take_delivered(&self) -> Vec<(u64, u64, u64)> {
        std::mem::take(&mut lock(&self.state).delivered)
    }
}

impl SinkTransport for SimLink {
    fn frames_v4(&self) -> bool {
        false
    }
    fn bulk_bucket_enabled(&self) -> bool {
        self.bucket.load(Ordering::SeqCst)
    }
    fn set_bulk_rate_inputs(&self, capacity_bps: u64, video_bps: u64) {
        let mut state = lock(&self.state);
        state.rate_inputs.push((capacity_bps, video_bps));
        state.bulk_rate = if capacity_bps == 0 {
            0
        } else {
            (capacity_bps.saturating_sub(video_bps) / 5 * 4).max(1_000_000) / 8
        };
    }
    fn now_us(&self) -> u64 {
        SimLink::now_us(self)
    }
    fn link_stats(&self) -> Option<LinkStats> {
        Some(LinkStats {
            bulk_bytes_sent: lock(&self.state).bulk_sent,
            ..LinkStats::default()
        })
    }
    fn enqueue(&self, message: Vec<u8>, ticket: SendTicket) -> Result<(), SendTicket> {
        let frame = (message[0] == 0x10)
            .then(|| ExtendedVideoPacket::decode(&message[5..], FrameVersion::V3).ok())
            .flatten()
            .filter(|p| p.metadata.flags != 2)
            .map(|p| (p.metadata.generation, p.metadata.sequence));
        let mut state = lock(&self.state);
        state.app.push_back(Message {
            bytes: message.len(),
            frame,
            bulk: false,
            ticket: Some(Arc::new(ticket)),
        });
        Ok(())
    }
}

/// A stream with rate adaptation over `link`, at `w`×`h` and 60 fps with the
/// plan's bitrate `bitrate_bps`.
fn adaptive(link: &Arc<SimLink>, plan: impl FnOnce(&mut StreamPlan)) -> Manual {
    let s = Manual::with_clock(link.clock.clone(), |fakes, parts| {
        fakes.factory.honour_rate.store(true, Ordering::SeqCst);
        parts.plan.pacing = PacingPolicy::with_rate_adaptation();
        parts.output = VideoOutput::with_transport(link.clone(), SinkPolicy::default(), None);
        plan(&mut parts.plan);
        if parts.plan.convert == ConvertPath::Convert {
            parts.converter = Some(fakes.converter.clone());
        }
    });
    s.start_running();
    s
}

#[derive(Default)]
struct Run {
    /// (ms, target bit/s, largest unsent age µs, fps, width) per 100 ms.
    trace: Vec<(u64, u64, u64, u32, u32)>,
    /// Frames submitted per second, per second.
    submitted: Vec<u64>,
}

/// Drive `s` over `link` for `seconds`: a 60 fps source (H0 = now), the
/// encoder answering at once, client feedback every 100 ms when `feedback`.
fn drive(s: &Manual, link: &SimLink, seconds: u64, feedback: bool, run: &mut Run) {
    let start = s.clock.now_ns();
    let mut next_frame = start;
    let mut last_submitted = s.controller.stats().submitted;
    let mut window_age = 0u32;
    for ms in 1..=seconds * 1_000 {
        let now = s.clock.now_ns();
        if now >= next_frame {
            let mut info = s.capture.surface(srgb(), now);
            info.source_present_ns = Some(now);
            s.capture.emit_info(info);
            next_frame += SECOND / 60;
        }
        s.pump();
        for encoder in lock(&s.factory.encoders).iter() {
            encoder.complete_all();
        }
        s.pump();
        window_age = window_age.max(s.controller.sink_state().oldest_unsent_age_ms);
        link.step();
        s.pump();
        if ms % 100 == 0 {
            if feedback {
                let delivered = link.take_delivered();
                let generation = delivered.last().map_or(0, |d| d.0);
                let frames = delivered
                    .iter()
                    .filter(|d| d.0 == generation)
                    .rev()
                    .take(32)
                    .map(|&(_, sequence, receive_us)| ClientFrameTimes {
                        sequence,
                        // The client's clock is 7 s ahead: only differences count.
                        receive_us: receive_us + 7_000_000,
                        decode_us: 0,
                        present_us: 0,
                    })
                    .collect();
                if generation != 0 {
                    s.controller.note_client_feedback(ClientFeedback {
                        generation,
                        loss_base_sequence: 0,
                        loss_bits: vec![],
                        frames,
                    });
                }
            }
            let stats = s.controller.stats();
            run.trace.push((
                ms,
                stats.abr_target_bps,
                u64::from(std::mem::take(&mut window_age)) * 1_000,
                stats.abr_fps,
                s.factory.last().spec.width,
            ));
        }
        if ms % 1_000 == 0 {
            let submitted = s.controller.stats().submitted;
            run.submitted.push(submitted - last_submitted);
            last_submitted = submitted;
        }
        s.queue.advance(MS);
    }
}

fn percentile(values: &mut [u64], p: usize) -> u64 {
    values.sort_unstable();
    values[(values.len() * p).div_ceil(100).max(1) - 1]
}

#[test]
fn converges_below_a_throttled_link_with_unsent_age_near_one_frame() {
    let clock = Arc::new(ManualClock(AtomicU64::new(SECOND)));
    // 8 Mbit/s behind a 16 KiB send buffer (TCP_NOTSENT_LOWAT-like): the queue
    // shows as the sink's unsent age.
    let link = SimLink::new(clock, 8_000_000, 16 * KIB);
    let s = adaptive(&link, |plan| {
        plan.encode_width = 1280;
        plan.encode_height = 720;
        plan.bitrate_bps = 20_000_000;
    });
    let mut run = Run::default();
    drive(&s, &link, 40, false, &mut run);
    let settled: Vec<_> = run.trace.iter().filter(|t| t.0 > 20_000).collect();
    let mean = settled.iter().map(|t| t.1).sum::<u64>() / settled.len() as u64;
    let max = settled.iter().map(|t| t.1).max().unwrap();
    let min = settled.iter().map(|t| t.1).min().unwrap();
    let mut ages: Vec<u64> = settled.iter().map(|t| t.2).collect();
    let p95 = percentile(&mut ages, 95);
    let stats = s.controller.stats();
    eprintln!(
        "throttled 8 Mbit/s, 16 KiB buffer: settled target mean {mean} (min {min}, max {max}) bit/s, \
         unsent age p95 {p95} µs, {} decreases, {} tier changes, fps {:?}",
        stats.abr_decreases, stats.abr_tier_changes, run.submitted
    );
    assert!(stats.abr_active);
    assert!(mean < 8_000_000, "converged below capacity");
    assert!(mean > 4_800_000, "not collapsed: {mean}");
    assert!(max <= 9_200_000, "{max}");
    assert!(
        p95 <= 17_000,
        "unsent age p95 {p95} µs within about one frame"
    );
    assert_eq!(
        stats.abr_tier_changes, 0,
        "720p60 fits 8 Mbit/s at its floor"
    );
    s.close();
    s.assert_clean();
}

#[test]
fn a_deep_send_buffer_is_seen_through_the_client_delay_and_credit_rtt() {
    let clock = Arc::new(ManualClock(AtomicU64::new(SECOND)));
    // 8 Mbit/s behind a 1 MiB send buffer: about a second of queue the sink
    // cannot see as unsent; the client's delay and the credit RTT do.
    let link = SimLink::new(clock, 8_000_000, 1024 * KIB);
    let s = adaptive(&link, |plan| {
        plan.encode_width = 1280;
        plan.encode_height = 720;
        plan.bitrate_bps = 20_000_000;
    });
    let mut run = Run::default();
    drive(&s, &link, 40, true, &mut run);
    let settled: Vec<_> = run.trace.iter().filter(|t| t.0 > 20_000).collect();
    let mean = settled.iter().map(|t| t.1).sum::<u64>() / settled.len() as u64;
    // One-way delay of frames over the last 20 s (written → received; the
    // sink's own queue is the unsent age).
    let times = lock(&link.state).video_times.clone();
    let mut delays: Vec<u64> = times[times.len() / 2..]
        .iter()
        .map(|(w, r)| r - w)
        .collect();
    let p95 = percentile(&mut delays, 95);
    let stats = s.controller.stats();
    eprintln!(
        "throttled 8 Mbit/s, 1 MiB buffer, feedback: settled target mean {mean} bit/s, \
         written→received p95 {p95} µs, {} client samples, {} decreases",
        stats.client_delay_samples, stats.abr_decreases
    );
    assert!(stats.client_feedback > 0 && stats.client_delay_samples > 0);
    assert!(mean < 8_000_000 && mean > 4_000_000, "{mean}");
    assert!(p95 < 60_000, "the hidden queue stays short: p95 {p95} µs");
    s.close();
    s.assert_clean();
}

#[test]
fn bulk_competition_keeps_the_floor_against_bulk() {
    // 14 Mbit/s behind a 256 KiB send buffer and no capacity hint: the bucket
    // runs on the sink's estimate (at least twice the video rate, 20 Mbit/s),
    // so bulk takes 8 Mbit/s, more than the link has left, fills the buffer
    // and video queues behind it. Rate adaptation cuts, but not below half of
    // its rate before bulk started.
    let clock = Arc::new(ManualClock(AtomicU64::new(SECOND)));
    let link = SimLink::new(clock, 14_000_000, 256 * KIB);
    link.bucket.store(true, Ordering::SeqCst);
    let s = adaptive(&link, |plan| {
        plan.encode_width = 1280;
        plan.encode_height = 720;
        plan.bitrate_bps = 10_000_000;
    });
    let mut run = Run::default();
    drive(&s, &link, 5, false, &mut run);
    let before = s.controller.stats().abr_target_bps;
    let decreases = s.controller.stats().abr_decreases;
    link.bulk(160, 256 * KIB);
    drive(&s, &link, 20, false, &mut run);
    let stats = s.controller.stats();
    let during: Vec<_> = run.trace.iter().filter(|t| t.0 > 5_000).collect();
    let low = during.iter().map(|t| t.1).min().unwrap();
    let state = lock(&link.state);
    let bulk_bps = state.bulk_delivered * 8 / 20;
    eprintln!(
        "14 Mbit/s, 256 KiB buffer, estimated capacity, bulk: target before {before}, lowest during \
         {low} bit/s, {} decreases during bulk; bulk {bulk_bps} bit/s",
        stats.abr_decreases - decreases
    );
    assert!(stats.abr_decreases > decreases, "bulk queueing was seen");
    assert!(low >= before / 2, "the floor against bulk holds: {low}");
    assert!(bulk_bps > 1_000_000, "bulk moves");
    assert_eq!(stats.abr_tier_changes, 0, "no tier step at the floor");
    drop(state);
    s.close();
    s.assert_clean();
}

#[test]
fn with_a_known_capacity_the_bucket_takes_the_target() {
    // A known 20 Mbit/s: bulk is held to (20 − video) × 0.8, video keeps its
    // rate and a short queue.
    let clock = Arc::new(ManualClock(AtomicU64::new(SECOND)));
    let link = SimLink::new(clock, 20_000_000, 16 * KIB);
    link.bucket.store(true, Ordering::SeqCst);
    let s = adaptive(&link, |plan| {
        plan.encode_width = 1280;
        plan.encode_height = 720;
        plan.bitrate_bps = 12_000_000;
        plan.medium_cap_bps = Some(20_000_000);
    });
    let mut run = Run::default();
    drive(&s, &link, 5, false, &mut run);
    link.bulk(160, 256 * KIB);
    drive(&s, &link, 20, false, &mut run);
    let stats = s.controller.stats();
    let during: Vec<_> = run.trace.iter().filter(|t| t.0 > 5_000).collect();
    let low = during.iter().map(|t| t.1).min().unwrap();
    let mut ages: Vec<u64> = during.iter().map(|t| t.2).collect();
    let p95 = percentile(&mut ages, 95);
    let state = lock(&link.state);
    let bulk_bps = state.bulk_delivered * 8 / 20;
    let inputs = state.rate_inputs.last().copied().unwrap();
    eprintln!(
        "20 Mbit/s known, bulk: lowest target {low} bit/s; bulk {bulk_bps} bit/s; unsent age p95 \
         {p95} µs; bucket inputs {inputs:?} (measured video {} bit/s)",
        stats.video_bps
    );
    assert!(low >= 10_000_000, "{low}");
    assert!(bulk_bps > 3_000_000, "bulk gets the rest");
    assert!(p95 <= 17_000, "unsent age p95 {p95} µs");
    // The bucket's video input is the target (at least the measured rate).
    assert_eq!(inputs.0, 20_000_000);
    assert!(stats.bulk_video_bps >= stats.abr_target_bps);
    drop(state);
    s.close();
    s.assert_clean();
}

#[test]
fn a_very_slow_link_steps_bitrate_then_fps_then_resolution() {
    let clock = Arc::new(ManualClock(AtomicU64::new(SECOND)));
    // 0.4 Mbit/s: below every 1080p tier's floor.
    let link = SimLink::new(clock, 400_000, 16 * KIB);
    let s = adaptive(&link, |plan| {
        plan.encode_width = 1920;
        plan.encode_height = 1080;
        plan.bitrate_bps = 13_700_000;
        plan.convert = ConvertPath::Convert;
    });
    let mut run = Run::default();
    drive(&s, &link, 60, false, &mut run);
    let first_fps = run
        .trace
        .iter()
        .position(|t| t.3 < 60)
        .expect("fps stepped");
    let first_size = run
        .trace
        .iter()
        .position(|t| t.4 < 1920)
        .expect("size stepped");
    let stats = s.controller.stats();
    eprintln!(
        "0.4 Mbit/s: fps step at {} ms (target {}), size step at {} ms (fps {}); final {:?}; \
         submitted per second {:?}",
        run.trace[first_fps].0,
        run.trace[first_fps].1,
        run.trace[first_size].0,
        run.trace[first_size].3,
        run.trace.last(),
        &run.submitted[run.submitted.len() - 5..]
    );
    // Bitrate first: the frame rate steps only once the target is below the
    // 60 fps floor (1920 × 1080 × 60 × 0.02 = 2.49 Mbit/s).
    assert!(run.trace[first_fps].1 < 2_490_000, "bitrate first");
    assert!(
        run.trace[..first_fps]
            .iter()
            .all(|t| t.3 == 60 && t.4 == 1920)
    );
    assert!(first_size > first_fps, "fps before resolution");
    assert_eq!(run.trace[first_size].3, abr::MIN_FPS);
    // The pacer admits the lower rate (the encoder is only told).
    assert!(*run.submitted.last().unwrap() <= u64::from(abr::MIN_FPS) + 1);
    // A resolution step is a new encoder generation at the smaller size.
    assert!(s.factory.count() >= 2);
    assert!(s.factory.last().spec.width < 1920);
    assert!(stats.abr_tier_changes >= 2);
    s.close();
    s.assert_clean();
}

#[test]
fn rates_reach_the_encoder_with_the_rate_window() {
    let clock = Arc::new(ManualClock(AtomicU64::new(SECOND)));
    let link = SimLink::new(clock, 4_000_000, 16 * KIB);
    let s = adaptive(&link, |plan| {
        plan.encode_width = 1280;
        plan.encode_height = 720;
        plan.bitrate_bps = 20_000_000;
        plan.pacing.max_frame_qp = 44;
    });
    let mut run = Run::default();
    drive(&s, &link, 5, false, &mut run);
    let rates = lock(&s.encoder().rates).clone();
    assert!(rates.len() >= 2, "rate changes reached the encoder");
    assert!(rates.iter().all(|r| r.rate_window_ms == Some(150)));
    assert!(rates.iter().all(|r| r.max_frame_qp == Some(44)));
    assert!(rates.last().unwrap().bitrate_bps < 20_000_000);
    s.close();
    s.assert_clean();
}

#[test]
fn switched_off_the_plan_rate_stands() {
    let clock = Arc::new(ManualClock(AtomicU64::new(SECOND)));
    let link = SimLink::new(clock, 4_000_000, 16 * KIB);
    let s = Manual::with_clock(link.clock.clone(), |fakes, parts| {
        fakes.factory.honour_rate.store(true, Ordering::SeqCst);
        parts.plan.bitrate_bps = 20_000_000;
        parts.output = VideoOutput::with_transport(link.clone(), SinkPolicy::default(), None);
    });
    s.start_running();
    let mut run = Run::default();
    drive(&s, &link, 3, false, &mut run);
    let stats = s.controller.stats();
    assert!(!stats.abr_active);
    assert_eq!(stats.abr_target_bps, 20_000_000);
    assert!(lock(&s.encoder().rates).is_empty());
    s.close();
    s.assert_clean();
}

#[test]
fn a_recovery_held_by_backpressure_runs_when_the_sink_is_ready() {
    // The sink's bounds on; a loss while frames are unsent and old.
    let s = Manual::with(|fakes, parts| {
        parts.output = VideoOutput::with_transport(fakes.out.clone(), SinkPolicy::default(), None);
    });
    s.start_running();
    let encoder = s.encoder();
    // Three frames in the encoder; the IDR goes into the transport, unwritten.
    s.frame(1_000);
    s.frame(2_000);
    s.frame(3_000);
    assert_eq!(encoder.pending_seqs(), vec![1, 2, 3]);
    encoder.complete(1);
    s.pump();
    // Older than a frame: backpressured.
    s.advance_ms(40);
    assert!(s.controller.sink_state().backpressured);
    // A loss: the encoder drops frame 2.
    encoder.drop_output(2, DropReason::NoOutput);
    s.pump();
    assert_eq!(s.controller.stats().recoveries, 0, "held by backpressure");
    let held_at = s.clock.now_ns();
    // The IDR is written and consumed: the sink is ready, the recovery runs now.
    while s.out.finish_any(0, true) {}
    s.pump();
    let stats = s.controller.stats();
    assert_eq!((stats.recoveries, stats.recoveries_on_ready), (1, 1));
    assert_eq!(s.clock.now_ns(), held_at, "no wait for the 500 ms re-check");
    let last = *encoder.submits.lock().unwrap().last().unwrap();
    assert!(last.force_keyframe);
    s.close();
    s.assert_clean();
}

#[test]
fn a_frame_the_rate_control_skipped_is_not_a_loss() {
    let s = Manual::new();
    s.out.sync_receipts.store(true, Ordering::SeqCst);
    s.start_running();
    let encoder = s.encoder();
    s.frame(1_000);
    encoder.complete_all();
    s.pump();
    // Frame 2 is skipped while frame 3 follows: nothing references 2.
    s.frame(2_000);
    s.frame(3_000);
    encoder.drop_output(2, DropReason::Skipped);
    encoder.complete(3);
    s.advance_ms(600);
    let stats = s.controller.stats();
    assert_eq!(
        (stats.encoder_skipped, stats.recoveries),
        (1, 0),
        "no recovery"
    );
    assert!(
        lock(&encoder.submits)
            .iter()
            .all(|o| o.submit_seq == 1 || !o.force_keyframe)
    );
    // The newest image skipped (a still screen): encoded again, as a delta.
    s.frame(4_000);
    encoder.drop_output(4, DropReason::Skipped);
    s.pump();
    assert_eq!(encoder.pending_seqs(), vec![5], "the last image again");
    assert!(!lock(&encoder.submits).last().unwrap().force_keyframe);
    encoder.complete(5);
    s.pump();
    // A skipped recovery keyframe leaves its loss: recovered again.
    s.frame(6_000);
    encoder.drop_output(6, DropReason::NoOutput);
    s.pump();
    let idr = *encoder.pending_seqs().last().unwrap();
    assert!(lock(&encoder.submits).last().unwrap().force_keyframe);
    encoder.drop_output(idr, DropReason::Skipped);
    s.advance_ms(600);
    let again = *encoder.pending_seqs().last().expect("recovered again");
    assert!(again > idr);
    assert!(lock(&encoder.submits).last().unwrap().force_keyframe);
    encoder.complete_all();
    s.pump();
    s.close();
    s.assert_clean();
}
