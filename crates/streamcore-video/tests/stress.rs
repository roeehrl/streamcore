//! Interleavings: seeded shuffles of every event source on a deterministic
//! queue, and bounded randomized runs with real threads. After every run: no
//! surface leaked or released twice, at most 3 frames in flight, no outbound
//! call outside a drain or after `Closed`, and a well-formed wire order.

mod support;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};
use streamcore_video::*;
use support::*;

fn assert_payloads_match_headers(fakes: &Fakes) {
    for packet in fakes.out.packets() {
        if !packet.meta.config {
            let seq = u64::from_le_bytes(packet.bytes[..8].try_into().expect("payload"));
            assert_eq!(seq, packet.meta.header.sequence);
            assert_eq!(packet.bytes[8] == 1, packet.meta.keyframe);
        }
    }
}

fn any_encoder(fakes: &Fakes, rng: &mut StdRng) -> Option<Arc<FakeEncoder>> {
    let count = fakes.factory.count();
    (count > 0).then(|| fakes.factory.encoder(rng.gen_range(0..count)))
}

fn random_update(rng: &mut StdRng) -> StreamUpdate {
    let size = rng.gen_bool(0.3).then(|| [32, 48, 64][rng.gen_range(0..3)]);
    StreamUpdate {
        rate: Some(RateUpdate {
            bitrate_bps: rng.gen_range(1_000_000..8_000_000),
            fps: rng.gen_range(1..=120),
        }),
        capture: CaptureUpdate {
            min_interval_ns: None,
            region: None,
            cursor: None,
        },
        encode_width: size,
        encode_height: size,
    }
}

#[test]
fn seeded_shuffles_keep_every_invariant() {
    for seed in 0..300u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let s = Manual::new();
        s.capture.auto_answer.store(true, Ordering::SeqCst);
        s.start_running();
        let mut capture_ns = 1_000;
        for _ in 0..rng.gen_range(50..300) {
            match rng.gen_range(0..100) {
                0..=29 => {
                    capture_ns += rng.gen_range(1..20_000_000);
                    let color = if rng.gen_bool(0.05) { p3() } else { srgb() };
                    s.capture.emit(color, capture_ns);
                }
                30..=49 => {
                    if let Some(e) = any_encoder(&s, &mut rng) {
                        let pending = e.pending_seqs();
                        if !pending.is_empty() {
                            e.complete(pending[rng.gen_range(0..pending.len())]);
                        }
                    }
                }
                50..=54 => {
                    if let Some(e) = any_encoder(&s, &mut rng) {
                        let pending = e.pending_seqs();
                        if !pending.is_empty() {
                            e.drop_output(pending[0], DropReason::NoOutput);
                        }
                    }
                }
                55..=69 => {
                    s.out.finish_any(rng.gen_range(0..8), rng.gen_bool(0.85));
                }
                70..=72 => {
                    let flip = !s.out.pressured.load(Ordering::SeqCst);
                    s.out.pressured.store(flip, Ordering::SeqCst);
                }
                73..=75 => s.controller.request_keyframe(),
                76..=77 => {
                    s.controller.update(random_update(&mut rng));
                }
                78..=79 => {
                    if let Some(e) = any_encoder(&s, &mut rng) {
                        e.busy_next.store(true, Ordering::SeqCst);
                    }
                }
                80..=89 => s.queue.advance(rng.gen_range(0..700) * MS),
                _ => s.pump(),
            }
            assert!(
                s.log.native_permits.load(Ordering::SeqCst) <= 3,
                "seed {seed}"
            );
            if rng.gen_bool(0.5) {
                s.pump();
            }
        }
        s.controller.begin_close();
        s.pump();
        for index in 0..s.factory.count() {
            s.factory.encoder(index).complete_all();
        }
        while s.out.finish_any(0, true) {}
        s.pump();
        assert_eq!(
            s.controller.close_outcome(),
            Some(CloseOutcome::Closed),
            "seed {seed}"
        );
        assert_eq!(
            s.log.native_permits.load(Ordering::SeqCst),
            0,
            "seed {seed}"
        );
        s.assert_clean();
        s.assert_wire_order();
        assert_payloads_match_headers(&s);
    }
}

#[test]
fn many_threads_against_one_stream_keep_every_invariant() {
    for seed in 0..6u64 {
        let fakes = Arc::new(Fakes::new());
        fakes.capture.auto_answer.store(true, Ordering::SeqCst);
        let port = StreamPort::new();
        let queue = ThreadQueue::new(port.clone());
        let controller =
            StreamController::new(fakes.parts(port, queue.clone(), plan())).expect("valid");
        controller.start();
        queue.flush();
        queue.flush();
        assert_eq!(controller.phase(), StreamPhase::Running);

        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(Barrier::new(5));
        let mut threads = Vec::new();
        // Capture callbacks, on the queue, plus a few from a foreign thread.
        {
            let (fakes, queue, stop, ready) =
                (fakes.clone(), queue.clone(), stop.clone(), ready.clone());
            threads.push(std::thread::spawn(move || {
                let mut rng = StdRng::seed_from_u64(seed);
                ready.wait();
                let mut capture_ns = 1_000u64;
                while !stop.load(Ordering::SeqCst) {
                    capture_ns += 8_000_000;
                    let color = if rng.gen_bool(0.03) { p3() } else { srgb() };
                    if rng.gen_bool(0.9) {
                        let capture = fakes.capture.clone();
                        queue.run_on(move || {
                            capture.emit(color, capture_ns);
                        });
                    } else {
                        fakes.capture.emit(color, capture_ns);
                    }
                    std::thread::sleep(Duration::from_micros(rng.gen_range(0..800)));
                }
            }));
        }
        // Encoder output threads.
        for worker in 0..2 {
            let (fakes, stop, ready) = (fakes.clone(), stop.clone(), ready.clone());
            threads.push(std::thread::spawn(move || {
                let mut rng = StdRng::seed_from_u64(seed * 31 + worker);
                ready.wait();
                while !stop.load(Ordering::SeqCst) {
                    if let Some(e) = any_encoder(&fakes, &mut rng) {
                        let pending = e.pending_seqs();
                        if !pending.is_empty() {
                            let seq = pending[rng.gen_range(0..pending.len())];
                            if rng.gen_bool(0.95) {
                                e.complete(seq);
                            } else {
                                e.drop_output(seq, DropReason::NoOutput);
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_micros(rng.gen_range(0..500)));
                }
            }));
        }
        // Send completions and consumer commands.
        {
            let (fakes, controller, stop, ready) = (
                fakes.clone(),
                controller.clone(),
                stop.clone(),
                ready.clone(),
            );
            threads.push(std::thread::spawn(move || {
                let mut rng = StdRng::seed_from_u64(seed * 17 + 5);
                ready.wait();
                while !stop.load(Ordering::SeqCst) {
                    fakes.out.finish_any(rng.gen_range(0..4), rng.gen_bool(0.9));
                    match rng.gen_range(0..40) {
                        0 => controller.request_keyframe(),
                        1 => controller.request_stats(),
                        2 => {
                            controller.update(random_update(&mut rng));
                        }
                        3 => {
                            let flip = !fakes.out.pressured.load(Ordering::SeqCst);
                            fakes.out.pressured.store(flip, Ordering::SeqCst);
                        }
                        _ => {}
                    }
                    std::thread::sleep(Duration::from_micros(rng.gen_range(0..600)));
                }
            }));
        }
        ready.wait();
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        let closer = controller.clone();
        let outcome = within(Duration::from_secs(10), move || {
            closer.close_and_wait(5_000)
        });
        assert_eq!(outcome, Ok(CloseOutcome::Closed), "seed {seed}");
        assert!(started.elapsed() < Duration::from_secs(2));
        stop.store(true, Ordering::SeqCst);
        for thread in threads {
            thread.join().expect("worker");
        }
        queue.flush();
        for index in 0..fakes.factory.count() {
            fakes.factory.encoder(index).complete_all();
        }
        while fakes.out.finish_any(0, true) {}
        queue.flush();
        assert!(
            fakes.out.samples().len() > 10,
            "seed {seed}: the stream made progress"
        );
        assert_eq!(fakes.log.native_permits.load(Ordering::SeqCst), 0);
        queue.quit();
        fakes.assert_clean();
        fakes.assert_wire_order();
        assert_payloads_match_headers(&fakes);
    }
}
