use streamcore_media::{
    media::{VideoMetadata, VideoPacket},
    stream::{VideoDisposition::*, VideoPacketizer, VideoReceiver},
};

fn packet(generation: u64, sequence: u64, flags: u8) -> VideoMetadata {
    VideoMetadata {
        stream_id: 7,
        generation,
        sequence,
        flags,
        pts_micros: if flags == 2 { 0 } else { sequence * 1000 },
    }
}

#[test]
fn capture_settings_reject_overflow_and_invalid_native_parameters() {
    use streamcore_media::stream::valid_video_settings as valid;
    assert!(valid(2, 1, 0.0));
    assert!(valid(8192, 120, 1.0));
    for dimension in [i64::MIN, -1, 0, 1, 8193, i64::MAX] {
        assert!(!valid(dimension, 60, 0.5));
    }
    for fps in [i64::MIN, -1, 0, 121, i64::MAX] {
        assert!(!valid(2732, fps, 0.5));
    }
    for quality in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.01, 1.01] {
        assert!(!valid(2732, 60, quality));
    }
}

#[test]
fn native_work_stays_bounded_until_output_consumption_and_close_is_final() {
    use streamcore_media::stream::VideoWorkQueue;
    let queue = VideoWorkQueue::new();
    let first = queue.acquire().unwrap();
    let second = queue.acquire().unwrap();
    let third = queue.acquire().unwrap();
    assert!(queue.acquire().is_none());
    first.release();
    first.release(); // duplicate native completion cannot create another slot
    let fourth = queue.acquire().unwrap();
    assert!(queue.acquire().is_none());
    drop(second); // abandoning output also frees admission
    let fifth = queue.acquire().unwrap();
    assert!(queue.acquire().is_none());
    queue.close();
    drop((third, fourth, fifth));
    assert!(queue.acquire().is_none());
    let replacement = VideoWorkQueue::new();
    let permits: Vec<_> = (0..3).map(|_| replacement.acquire().unwrap()).collect();
    drop(first); // stale old-encoder release cannot free the replacement's work
    assert!(replacement.acquire().is_none());
    drop(permits);
    assert!(replacement.acquire().is_some());
}

#[test]
fn concurrent_native_backends_never_exceed_three_inflight_frames() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use streamcore_media::stream::VideoWorkQueue;
    let queue = Arc::new(VideoWorkQueue::new());
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for _ in 0..12 {
            let (queue, active, peak) = (queue.clone(), active.clone(), peak.clone());
            scope.spawn(move || {
                for _ in 0..1_000 {
                    if let Some(permit) = queue.acquire() {
                        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(count, Ordering::SeqCst);
                        std::thread::yield_now();
                        active.fetch_sub(1, Ordering::SeqCst);
                        permit.release();
                    }
                }
            });
        }
    });
    assert!((1..=3).contains(&peak.load(Ordering::SeqCst)));
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[test]
fn a_dropped_delta_requires_a_new_keyframe() {
    let r = VideoReceiver::new(7);
    assert_eq!(r.accept(packet(1, 0, 2)), Configure);
    assert_eq!(r.accept(packet(1, 1, 1)), Decode);
    assert_eq!(r.accept(packet(1, 2, 0)), Decode);
    assert_eq!(r.accept(packet(1, 4, 0)), Recover);
    assert_eq!(r.accept(packet(1, 5, 0)), Recover);
    assert_eq!(r.accept(packet(1, 3, 1)), Ignore); // late keyframe cannot repair newer dependencies
    assert_eq!(r.accept(packet(1, 0, 2)), Ignore); // repeated config doesn't reset the chain
    assert_eq!(r.accept(packet(1, 6, 1)), Decode);
    assert_eq!(r.accept(packet(1, 7, 0)), Decode);
    r.require_keyframe();
    assert_eq!(r.accept(packet(1, 8, 0)), Recover);
    assert_eq!(r.accept(packet(1, 9, 1)), Decode);
}

#[test]
fn late_encoder_callbacks_and_missing_config_cannot_rewind_stream() {
    let r = VideoReceiver::new(7);
    assert_eq!(r.accept(packet(10, 0, 2)), Configure);
    assert_eq!(r.accept(packet(10, 1, 1)), Decode);
    assert_eq!(r.accept(packet(11, 1, 1)), Recover);
    assert_eq!(r.accept(packet(10, 0, 2)), Ignore);
    assert_eq!(r.accept(packet(10, 2, 1)), Ignore);
    assert_eq!(r.accept(packet(11, 0, 2)), Configure);
    assert_eq!(r.accept(packet(11, 2, 0)), Recover);
    r.reject_config();
    assert_eq!(r.accept(packet(11, 3, 1)), Recover);
    assert_eq!(r.accept(packet(11, 0, 2)), Configure);
    assert_eq!(r.accept(packet(11, 4, 1)), Decode);
    let mut foreign = packet(999, 0, 2);
    foreign.stream_id = 8;
    assert_eq!(r.accept(foreign), Ignore);
    assert_eq!(r.accept(packet(11, 5, 0)), Decode);
}

#[test]
fn packetizer_keeps_submission_gaps_and_changes_generation_on_restart_or_format() {
    let p = VideoPacketizer::new(7);
    let first = p.next(true, true, 1, 1000).unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!((first[0].flags, first[0].sequence), (2, 0));
    let delta = p.next(false, false, 3, 3000).unwrap();
    assert_eq!(delta.len(), 1);
    assert_eq!(delta[0].sequence, 3);
    assert_eq!(delta[0].generation, first[0].generation);
    assert!(p.next(true, true, 2, 2000).unwrap().is_empty());
    let key = p.next(true, false, 4, 4000).unwrap();
    assert_eq!(key[0], first[0]);
    let changed = p.next(true, true, 5, 5000).unwrap();
    assert!(changed[0].generation > first[0].generation);
    let restarted = VideoPacketizer::new(7).next(true, true, 1, 0).unwrap();
    assert!(restarted[0].generation > changed[0].generation);
}

#[test]
fn binary_headers_are_bounded_strict_and_borrow_native_payload() {
    let packet = VideoPacket {
        metadata: packet(42, 1, 1),
        data: &[1, 2, 3],
    };
    let bytes = packet.encode().unwrap();
    let decoded = VideoPacket::decode(&bytes).unwrap();
    assert_eq!(decoded, packet);
    assert_eq!(decoded.data.as_ptr(), bytes[29..].as_ptr());
    for length in 0..=29 {
        assert!(VideoPacket::decode(&bytes[..length]).is_err());
    }
    for flags in [3, 4, 255] {
        let mut invalid = bytes.clone();
        invalid[4] = flags;
        assert!(VideoPacket::decode(&invalid).is_err());
    }
    let mut exhausted = packet.metadata;
    exhausted.sequence = u64::MAX;
    let r = VideoReceiver::new(7);
    assert_eq!(r.accept(super_config(exhausted)), Configure);
    assert_eq!(r.accept(exhausted), Decode);
    assert_eq!(r.accept(exhausted), Ignore);
}
fn super_config(mut m: VideoMetadata) -> VideoMetadata {
    m.flags = 2;
    m.sequence = 0;
    m.pts_micros = 0;
    m
}

#[test]
fn interleaved_120_fps_streams_recover_independently() {
    let senders: Vec<_> = (1..=8).map(VideoPacketizer::new).collect();
    let receivers: Vec<_> = (1..=8).map(VideoReceiver::new).collect();
    // Ten seconds of eight streams at 120 fps. Intentional packet loss on one
    // stream must never suppress decoding on another. This is policy testing,
    // not a capture/render performance measurement.
    for sequence in 1..=1200 {
        for (index, (sender, receiver)) in senders.iter().zip(&receivers).enumerate() {
            let key = sequence % 120 == 1;
            for packet in sender.next(key, false, sequence, sequence * 8333).unwrap() {
                if index == 0 && sequence == 30 {
                    continue;
                }
                let disposition = receiver.accept(packet);
                let expected = if packet.flags == 2 {
                    if sequence == 1 { Configure } else { Ignore }
                } else if index == 0 && (31..=120).contains(&sequence) {
                    Recover
                } else {
                    Decode
                };
                assert_eq!(disposition, expected, "stream {index} sequence {sequence}");
            }
        }
    }
}

#[test]
fn every_permit_release_calls_the_hook_once_and_close_races_admit_nothing() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use streamcore_media::stream::VideoWorkQueue;
    let released = Arc::new(AtomicUsize::new(0));
    let counter = released.clone();
    let queue = VideoWorkQueue::with_release_hook(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    let first = queue.acquire().unwrap();
    let second = queue.acquire().unwrap();
    assert_eq!(queue.in_flight(), 2);
    first.release();
    first.release();
    drop(first);
    assert_eq!(released.load(Ordering::SeqCst), 1);
    drop(second);
    assert_eq!(released.load(Ordering::SeqCst), 2);
    assert_eq!(queue.in_flight(), 0);

    // An acquire racing a close either wins before it or gets nothing after it.
    for _ in 0..200 {
        let queue = Arc::new(VideoWorkQueue::new());
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        std::thread::scope(|scope| {
            let (q, c) = (queue.clone(), closed.clone());
            scope.spawn(move || {
                q.close();
                c.store(true, Ordering::SeqCst);
            });
            for _ in 0..50 {
                let was_closed = closed.load(Ordering::SeqCst);
                let permit = queue.acquire();
                assert!(!(was_closed && permit.is_some()));
            }
        });
        assert!(queue.is_closed() && queue.acquire().is_none());
    }
}
