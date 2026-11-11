//! Typed ping and pong (`clock.v1`) and the peer clock estimate.
//!
//! When both peers negotiate [`CLOCK_CAPABILITY`], the multiplexer answers every
//! typed ping on lane 0 itself, stamping the pong with its receive time (when the
//! ping's last fragment arrived) and its send time (when the pong's first fragment
//! is handed to the writer). A local ping's send time is stamped the same way.
//! Pings and pongs are still delivered to the application as liveness signals; the
//! application never answers them.
//!
//! The estimate is NTP-style: each answered ping gives an offset (peer clock minus
//! local clock) whose error is at most half its round trip. The estimate uses the
//! minimum-round-trip sample of the last 10 s and fits drift over the samples kept.
//! Times are microseconds of this process's monotonic clock ([`monotonic_now_us`]);
//! native code maps its own monotonic clock onto it once.
use std::{
    collections::VecDeque,
    sync::{Mutex, OnceLock},
    time::Instant,
};
use streamcore_media::binary::ClockPong;

/// Transport Hello capability for typed ping and pong. Consumers require it: the
/// typed bodies are a hard break with the untyped echo.
pub const CLOCK_CAPABILITY: &str = "clock.v1";

/// Offset samples are chosen from this window.
const WINDOW_US: u64 = 10_000_000;
/// Drift is fitted over at most this many recent samples.
const SAMPLES: usize = 256;
/// Unanswered pings remembered for matching their pongs.
const OUTSTANDING: usize = 32;

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Microseconds on this process's monotonic clock (not wall time; it starts at an
/// arbitrary point). Native trace points map their own monotonic clock onto it.
#[uniffi::export]
pub fn monotonic_now_us() -> u64 {
    u64::try_from(EPOCH.get_or_init(Instant::now).elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// The peer clock relative to ours, from typed pings this side sent.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct ClockEstimate {
    /// Answered pings kept (at most 256).
    pub samples: u32,
    /// Peer clock minus local clock, in microseconds, from the minimum-round-trip
    /// sample of the last 10 s (or the newest sample when none is that recent).
    pub offset_us: i64,
    /// The offset is within ± this bound: half the chosen sample's round trip.
    pub error_us: u64,
    /// The chosen sample's round trip, excluding the peer's hold time.
    pub rtt_us: u64,
    /// The newest sample's round trip.
    pub last_rtt_us: u64,
    /// Local time (`monotonic_now_us`) at which the chosen sample was taken.
    pub sampled_at_us: u64,
    /// Fitted drift of the peer clock against ours, in parts per million; zero
    /// until two well-separated low-delay samples exist.
    pub drift_ppm: f64,
}

#[derive(Clone, Copy)]
struct Sample {
    at_us: u64,
    rtt_us: u64,
    offset_us: i64,
}

/// Offset sample from one exchange: local send `t1`, peer receive `t2`, peer send
/// `t3`, local receive `t4`. None when the times are inconsistent.
fn sample(t1: u64, pong: ClockPong, t4: u64) -> Option<Sample> {
    let held = pong.send_us.checked_sub(pong.receive_us)?;
    let rtt = t4.checked_sub(t1)?.checked_sub(held)?;
    let t1 = i128::from(t1);
    let t4 = i128::from(t4);
    let offset = (i128::from(pong.receive_us) - t1 + i128::from(pong.send_us) - t4) / 2;
    Some(Sample {
        at_us: u64::try_from(t4).ok()?,
        rtt_us: rtt,
        offset_us: i64::try_from(offset).ok()?,
    })
}

#[derive(Default)]
struct State {
    next_nonce: u64,
    outstanding: VecDeque<(u64, u64)>,
    samples: VecDeque<Sample>,
}

#[derive(Default)]
pub(crate) struct Clock {
    state: Mutex<State>,
}

impl Clock {
    pub(crate) fn next_nonce(&self) -> u64 {
        let Ok(mut state) = self.state.lock() else {
            return 0;
        };
        state.next_nonce = state.next_nonce.wrapping_add(1);
        state.next_nonce
    }
    /// Records the send time stamped into a ping as it is written.
    pub(crate) fn sent(&self, nonce: u64, send_us: u64) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.outstanding.len() == OUTSTANDING {
            state.outstanding.pop_front();
        }
        state.outstanding.push_back((nonce, send_us));
    }
    /// Matches a pong to its ping. Unknown or repeated nonces are ignored.
    pub(crate) fn answered(&self, pong: ClockPong, receive_us: u64) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(index) = state.outstanding.iter().position(|(n, _)| *n == pong.nonce) else {
            return;
        };
        let Some((_, sent)) = state.outstanding.remove(index) else {
            return;
        };
        if let Some(sample) = sample(sent, pong, receive_us) {
            if state.samples.len() == SAMPLES {
                state.samples.pop_front();
            }
            state.samples.push_back(sample);
        }
    }
    pub(crate) fn estimate(&self, now_us: u64) -> Option<ClockEstimate> {
        let state = self.state.lock().ok()?;
        estimate(&state.samples, now_us)
    }
}

fn estimate(samples: &VecDeque<Sample>, now_us: u64) -> Option<ClockEstimate> {
    let last = samples.back()?;
    let recent = samples
        .iter()
        .filter(|s| now_us.saturating_sub(s.at_us) <= WINDOW_US)
        .min_by_key(|s| s.rtt_us)
        .unwrap_or(last);
    Some(ClockEstimate {
        samples: u32::try_from(samples.len()).unwrap_or(u32::MAX),
        offset_us: recent.offset_us,
        error_us: recent.rtt_us.div_ceil(2),
        rtt_us: recent.rtt_us,
        last_rtt_us: last.rtt_us,
        sampled_at_us: recent.at_us,
        drift_ppm: drift_ppm(samples),
    })
}

/// Least-squares slope of the low-delay samples' offsets over time. Samples whose
/// round trip exceeds the minimum by more than half (plus 200 µs) carry queueing
/// noise and are left out.
fn drift_ppm(samples: &VecDeque<Sample>) -> f64 {
    let Some(min) = samples.iter().map(|s| s.rtt_us).min() else {
        return 0.0;
    };
    let limit = min + min / 2 + 200;
    let good: Vec<&Sample> = samples.iter().filter(|s| s.rtt_us <= limit).collect();
    let (Some(first), Some(last)) = (good.first(), good.last()) else {
        return 0.0;
    };
    // Below a second of spread the slope is noise.
    if good.len() < 2 || last.at_us.saturating_sub(first.at_us) < 1_000_000 {
        return 0.0;
    }
    let n = good.len() as f64;
    let x0 = first.at_us as f64;
    let mean_x = good.iter().map(|s| s.at_us as f64 - x0).sum::<f64>() / n;
    let mean_y = good.iter().map(|s| s.offset_us as f64).sum::<f64>() / n;
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for s in &good {
        let dx = s.at_us as f64 - x0 - mean_x;
        sxy += dx * (s.offset_us as f64 - mean_y);
        sxx += dx * dx;
    }
    if sxx == 0.0 { 0.0 } else { sxy / sxx * 1e6 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pong(nonce: u64, receive_us: u64, send_us: u64) -> ClockPong {
        ClockPong {
            nonce,
            receive_us,
            send_us,
        }
    }

    #[test]
    fn offset_and_error_come_from_the_minimum_round_trip_of_the_window() {
        let clock = Clock::default();
        // Peer clock = local + 5_000 µs. Symmetric 1 ms paths give the exact offset.
        clock.sent(1, 100_000);
        clock.answered(pong(1, 106_000, 106_050), 102_050);
        // A queued sample: 9 ms out, 1 ms back. Its offset is off by 4 ms.
        clock.sent(2, 200_000);
        clock.answered(pong(2, 214_000, 214_000), 210_000);
        let e = clock.estimate(210_000).unwrap();
        assert_eq!(
            (e.samples, e.offset_us, e.rtt_us, e.error_us),
            (2, 5_000, 2_000, 1_000)
        );
        assert_eq!(e.last_rtt_us, 10_000);
        // Once both have left the window, the newest sample stands in.
        let later = clock.estimate(210_000 + WINDOW_US + 1).unwrap();
        assert_eq!(later.offset_us, 9_000);
        assert_eq!(later.error_us, 5_000);
    }

    #[test]
    fn unknown_repeated_and_inconsistent_pongs_add_no_sample() {
        let clock = Clock::default();
        clock.answered(pong(9, 10, 10), 20);
        assert!(clock.estimate(20).is_none());
        clock.sent(1, 1_000);
        // Held longer than the whole round trip: inconsistent.
        clock.answered(pong(1, 0, 5_000), 2_000);
        clock.answered(pong(1, 1_500, 1_500), 2_000);
        assert!(clock.estimate(2_000).is_none());
        for nonce in 0..(OUTSTANDING as u64 + 1) {
            clock.sent(nonce + 10, 0);
        }
        clock.answered(pong(10, 5, 5), 10); // forgotten: the oldest was evicted
        assert!(clock.estimate(10).is_none());
        clock.answered(pong(11, 5, 5), 10);
        assert_eq!(clock.estimate(10).unwrap().offset_us, 0);
    }

    #[test]
    fn drift_is_fitted_over_low_delay_samples() {
        let clock = Clock::default();
        // The peer gains 50 ppm: +50 µs per second.
        for i in 0..20_u64 {
            let t1 = i * 1_000_000;
            let offset = 1_000 + i * 50;
            let extra = if i % 5 == 0 { 20_000 } else { 0 }; // queued outliers
            clock.sent(i + 1, t1);
            let t2 = t1 + 500 + offset;
            clock.answered(pong(i + 1, t2, t2), t1 + 1_000 + extra);
        }
        let e = clock.estimate(19_000_000).unwrap();
        assert!((e.drift_ppm - 50.0).abs() < 1.0, "{}", e.drift_ppm);
        assert_eq!(e.rtt_us, 1_000);
    }

    #[test]
    fn the_monotonic_clock_never_goes_backwards() {
        let a = monotonic_now_us();
        let b = monotonic_now_us();
        assert!(b >= a);
    }
}
