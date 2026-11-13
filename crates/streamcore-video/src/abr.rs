//! Rate adaptation (STREAMCORE-VIDEO.md 6.6): a delay-based controller,
//! identical on every medium (TCP and the USB relays hide loss, so loss is only
//! one input among several).
//!
//! Inputs, sampled by the executor about every 100 ms ([`AbrSample`]) and from
//! the client's `videoFeedback` ([`AbrController::note_client_delay`]):
//!
//! - the sink's unsent age (the oldest frame not yet written to the transport);
//! - the consumed-credit RTT above its windowed minimum;
//! - the client's one-way delay above its windowed minimum and its gradient
//!   (receive time minus the frame's H5, so the unknown clock offset cancels);
//! - loss (frames the sink could not send or shed as aged, frames the client
//!   reported missing) and, on QUIC, quinn's lost packets and its RTT above
//!   its windowed minimum.
//!
//! Queueing is the largest of the delay inputs. Above one frame interval (or
//! on new loss) the target drops to ×0.85 of the smaller of the target and
//! the measured video rate, at most once per `max(250 ms, 2 × RTT)`; between
//! half a frame and a frame, or while the client delay grows, it holds;
//! otherwise it rises by 6 % per second (6.6: 5–8 %/s), unless the encoder
//! is application-limited (a still screen proves nothing about the link).
//!
//! The ladder is bitrate, then frame rate, then resolution (a new encoder
//! generation): a step down happens when the target falls below the current
//! tier's quality floor (`w·h·fps·0.02`), a step up (in the reverse order)
//! once the target has room for the next tier (at least twice its floor, or
//! this tier's ceiling) after a quiet period. The bitrate of a tier is
//! `w·h·fps·(0.02 + 0.18q)`; the plan's bitrate is that at full size, so each
//! tier's ceiling scales the plan's bitrate by its pixel rate, clamped to
//! 2–80 Mbit/s and the medium cap.
//!
//! The floor against bulk: when bulk transfer starts competing, the target
//! keeps at least half of what it was (and, with a known link capacity, 40 %
//! of it). Without it, cutting video would hand the bulk token bucket (6.4
//! rule 5) more room, whose bulk then queues ahead of video again: a spiral
//! down to the 2 Mbit/s minimum.
//!
//! The controller is a pure state machine on host times, unit-tested without
//! an executor, like [`crate::pacing`].

use std::collections::VecDeque;

/// The lowest tier ceiling (6.6: a tier's bitrate is clamped to 2–80 Mbit/s).
pub const MIN_BITRATE_BPS: u64 = 2_000_000;
/// The lowest target. Below a tier's 2 Mbit/s the ladder lowers the frame
/// rate and then the resolution, whose quality floors (`w·h·fps·0.02`) go
/// below 2 Mbit/s: 1920 × 1080 at 15 fps is 0.62 Mbit/s, at half size 0.16.
pub const MIN_TARGET_BPS: u64 = 250_000;
/// The highest target (6.6).
pub const MAX_BITRATE_BPS: u64 = 80_000_000;
/// The multiplicative decrease (6.6).
pub const DECREASE: f64 = 0.85;
/// The increase per second while the link shows no queueing (6.6: 5–8 %).
pub const INCREASE_PER_SECOND: f64 = 0.06;
/// The lowest frame rate the ladder steps down to.
pub const MIN_FPS: u32 = 15;
/// Frame rates the ladder steps through below the configured one.
const FPS_LEVELS: [u32; 6] = [90, 60, 45, 30, 20, 15];
/// Resolution steps, as a fraction of the configured encode size (per edge).
const SCALE_LEVELS: [(u32, u32); 3] = [(1, 1), (3, 4), (1, 2)];
/// Bits per pixel at quality 0, the floor of a tier (`0.02` in 6.6).
const FLOOR_BITS_PER_PIXEL: f64 = 0.02;
/// How long the windowed minima remember (RTT and one-way delay).
const MIN_WINDOW_NS: u64 = 10_000_000_000;
/// Client delay older than this is ignored.
const CLIENT_STALE_NS: u64 = 600_000_000;
/// The span of the client delay gradient.
const GRADIENT_SPAN_US: u64 = 500_000;
/// A one-way delay rising faster than this (µs per µs) holds the rate.
const GRADIENT_HOLD: f64 = 0.02;
/// Decreases are at least this far apart (and at least 2 × RTT).
const DECREASE_GAP_NS: u64 = 250_000_000;
/// Quiet time before the frame rate steps back up.
const FPS_UP_QUIET_NS: u64 = 2_000_000_000;
/// Quiet time before the resolution steps back up (it costs an IDR).
const RESOLUTION_UP_QUIET_NS: u64 = 5_000_000_000;
/// Below this share of the target the encoder is application-limited.
const APP_LIMITED: f64 = 0.5;
/// The measured video rate is a one-second window: it means nothing earlier.
const RATE_WARMUP_NS: u64 = 1_000_000_000;

/// What the controller adapts: the plan's encode size, frame rate and bitrate
/// (the full tier), and whether resolution steps are possible (the stream
/// scales through a converter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbrConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// The full tier's bitrate, `w·h·fps·(0.02 + 0.18q)` with the consumer's
    /// ceiling applied.
    pub bitrate_bps: u64,
    /// The medium cap.
    pub medium_cap_bps: Option<u64>,
    /// The encode size can change (a new encoder generation).
    pub resolution_steps: bool,
}

/// One tick's inputs. Cumulative counters only ever grow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AbrSample {
    /// Host time.
    pub now_ns: u64,
    /// Age of the oldest frame not yet written (0 when none).
    pub unsent_age_ns: u64,
    /// The smallest consumed-credit RTT of the last few hundred ms.
    pub credit_rtt_ns: Option<u64>,
    /// The QUIC path's smoothed RTT (quinn).
    pub link_rtt_ns: Option<u64>,
    /// QUIC packets lost so far (quinn), cumulative.
    pub link_lost_packets: u64,
    /// Video frames lost so far (refused, shed as aged, or reported missing
    /// by the client), cumulative.
    pub lost_frames: u64,
    /// The video rate measured over the last second.
    pub video_bps: u64,
    /// Bulk data was sent recently.
    pub bulk_active: bool,
    /// The link capacity, when known (not the sink's estimate).
    pub capacity_bps: Option<u64>,
}

/// The controller's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbrTarget {
    pub bitrate_bps: u64,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
}

/// Why the last tick moved (or held) the rate; for statistics and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AbrState {
    #[default]
    Increase,
    Hold,
    Decrease,
    /// Application-limited: the encoder produces well below the target.
    AppLimited,
}

/// A windowed minimum over (time, value) samples.
#[derive(Debug, Clone, Default)]
struct WindowMin {
    samples: VecDeque<(u64, i64)>,
}

impl WindowMin {
    fn push(&mut self, now: u64, value: i64) {
        while self.samples.back().is_some_and(|(_, v)| *v >= value) {
            self.samples.pop_back();
        }
        self.samples.push_back((now, value));
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.saturating_sub(*at) > MIN_WINDOW_NS)
        {
            self.samples.pop_front();
        }
    }

    fn min(&self) -> Option<i64> {
        self.samples.front().map(|(_, v)| *v)
    }

    fn clear(&mut self) {
        self.samples.clear();
    }
}

/// The rate adaptation controller. See the module documentation.
#[derive(Debug, Clone)]
pub struct AbrController {
    config: AbrConfig,
    fps_levels: Vec<u32>,
    fps_index: usize,
    scale_index: usize,
    /// The target in bit/s, kept as a float so small increases accumulate.
    target: f64,
    last_tick_ns: Option<u64>,
    last_decrease_ns: Option<u64>,
    last_step_ns: u64,
    credit_min: WindowMin,
    link_min: WindowMin,
    client_min: WindowMin,
    /// (send µs, one-way µs) of the client's recent frames.
    client_points: VecDeque<(u64, i64)>,
    /// (host time, excess ns) of the last client report.
    client_excess: Option<(u64, u64)>,
    lost_frames: u64,
    link_lost: u64,
    bulk_floor: Option<f64>,
    first_tick_ns: Option<u64>,
    /// The previous tick saw queueing above one frame.
    queued: bool,
    state: AbrState,
    queueing_ns: u64,
    decreases: u64,
    tier_changes: u64,
}

impl AbrController {
    pub fn new(config: AbrConfig) -> Self {
        let fps = config.fps.clamp(1, 240);
        let mut fps_levels = vec![fps];
        fps_levels.extend(FPS_LEVELS.iter().copied().filter(|level| *level < fps));
        let mut controller = Self {
            config: AbrConfig { fps, ..config },
            fps_levels,
            fps_index: 0,
            scale_index: 0,
            target: 0.0,
            last_tick_ns: None,
            last_decrease_ns: None,
            last_step_ns: 0,
            credit_min: WindowMin::default(),
            link_min: WindowMin::default(),
            client_min: WindowMin::default(),
            client_points: VecDeque::new(),
            client_excess: None,
            lost_frames: 0,
            link_lost: 0,
            bulk_floor: None,
            first_tick_ns: None,
            queued: false,
            state: AbrState::Increase,
            queueing_ns: 0,
            decreases: 0,
            tier_changes: 0,
        };
        controller.target = controller.cap(0, 0);
        controller
    }

    pub fn config(&self) -> AbrConfig {
        self.config
    }

    /// A new configuration (the consumer's rate or size update): the ladder
    /// starts again at the full tier, keeping the current target where it
    /// fits.
    pub fn reconfigure(&mut self, config: AbrConfig) {
        let target = self.target;
        let lost = (self.lost_frames, self.link_lost);
        *self = Self::new(config);
        self.target = target.clamp(self.floor_abs(), self.cap(0, 0));
        (self.lost_frames, self.link_lost) = lost;
    }

    pub fn target(&self) -> AbrTarget {
        let (width, height) = self.size(self.scale_index);
        AbrTarget {
            bitrate_bps: self.target as u64,
            fps: self.fps_levels[self.fps_index],
            width,
            height,
        }
    }

    pub fn state(&self) -> AbrState {
        self.state
    }

    /// The queueing delay the last tick saw.
    pub fn queueing_ns(&self) -> u64 {
        self.queueing_ns
    }

    pub fn decreases(&self) -> u64 {
        self.decreases
    }

    /// Frame rate and resolution steps so far.
    pub fn tier_changes(&self) -> u64 {
        self.tier_changes
    }

    /// The client's per-frame receive times from `videoFeedback`, each paired
    /// with the frame's H5 on the host's transport clock: `(h5_us,
    /// receive_us)`. The difference carries an unknown, constant clock offset
    /// that the windowed minimum removes.
    pub fn note_client_delay(&mut self, now_ns: u64, samples: &[(u64, u64)]) {
        if samples.is_empty() {
            return;
        }
        let mut excess = Vec::with_capacity(samples.len());
        for &(h5_us, receive_us) in samples {
            let owd = i64::try_from(receive_us).unwrap_or(i64::MAX)
                - i64::try_from(h5_us).unwrap_or(i64::MAX);
            self.client_min.push(now_ns, owd);
            self.client_points.push_back((h5_us, owd));
        }
        let newest = samples.iter().map(|(h5, _)| *h5).max().unwrap_or(0);
        while self
            .client_points
            .front()
            .is_some_and(|(h5, _)| newest.saturating_sub(*h5) > GRADIENT_SPAN_US)
        {
            self.client_points.pop_front();
        }
        let min = self.client_min.min().unwrap_or(0);
        for &(h5_us, receive_us) in samples {
            let owd = i64::try_from(receive_us).unwrap_or(i64::MAX)
                - i64::try_from(h5_us).unwrap_or(i64::MAX);
            excess.push(u64::try_from(owd - min).unwrap_or(0));
        }
        // The median: one large frame (an IDR) takes longer to arrive without
        // any queue.
        excess.sort_unstable();
        let median = excess[excess.len() / 2];
        self.client_excess = Some((now_ns, median.saturating_mul(1_000)));
    }

    /// The client's one-way delay slope (µs per µs) over the last 500 ms.
    pub fn client_gradient(&self) -> Option<f64> {
        if self.client_points.len() < 4 {
            return None;
        }
        let n = self.client_points.len() as f64;
        let x0 = self.client_points[0].0 as f64;
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
        for &(x, y) in &self.client_points {
            let x = x as f64 - x0;
            let y = y as f64;
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
        }
        let denominator = n * sxx - sx * sx;
        if denominator <= 0.0 {
            return None;
        }
        Some((n * sxy - sx * sy) / denominator)
    }

    /// One tick. Returns the new target when it changed.
    pub fn tick(&mut self, sample: &AbrSample) -> Option<AbrTarget> {
        let before = self.target();
        let now = sample.now_ns;
        let elapsed = self
            .last_tick_ns
            .map_or(0, |last| now.saturating_sub(last))
            .min(500_000_000);
        self.last_tick_ns = Some(now);
        let first = *self.first_tick_ns.get_or_insert(now);
        // Before the measured rate has a full window, it says nothing about
        // the encoder's output: take the target.
        let video_bps = if now.saturating_sub(first) < RATE_WARMUP_NS {
            self.target as u64
        } else {
            sample.video_bps
        };
        let interval = 1_000_000_000 / u64::from(self.fps_levels[self.fps_index].max(1));

        // Delay inputs above their windowed minima.
        let credit_excess = sample.credit_rtt_ns.map_or(0, |rtt| {
            let rtt = i64::try_from(rtt).unwrap_or(i64::MAX);
            self.credit_min.push(now, rtt);
            u64::try_from(rtt - self.credit_min.min().unwrap_or(rtt)).unwrap_or(0)
        });
        let link_excess = sample.link_rtt_ns.map_or(0, |rtt| {
            let rtt = i64::try_from(rtt).unwrap_or(i64::MAX);
            self.link_min.push(now, rtt);
            u64::try_from(rtt - self.link_min.min().unwrap_or(rtt)).unwrap_or(0)
        });
        let client_excess = self
            .client_excess
            .filter(|(at, _)| now.saturating_sub(*at) <= CLIENT_STALE_NS)
            .map_or(0, |(_, excess)| excess);
        let queueing = sample
            .unsent_age_ns
            .max(credit_excess)
            .max(link_excess)
            .max(client_excess);
        self.queueing_ns = queueing;
        let loss =
            sample.lost_frames > self.lost_frames || sample.link_lost_packets > self.link_lost;
        self.lost_frames = self.lost_frames.max(sample.lost_frames);
        self.link_lost = self.link_lost.max(sample.link_lost_packets);
        let growing = self
            .client_gradient()
            .is_some_and(|slope| slope > GRADIENT_HOLD)
            && client_excess > 0;

        // The floor against bulk.
        if sample.bulk_active {
            if self.bulk_floor.is_none() {
                let mut floor = self.target * 0.5;
                if let Some(capacity) = sample.capacity_bps {
                    floor = floor.max(capacity as f64 * 0.4);
                }
                self.bulk_floor = Some(floor);
            }
        } else {
            self.bulk_floor = None;
        }

        let rtt = sample
            .credit_rtt_ns
            .or(sample.link_rtt_ns)
            .unwrap_or(0)
            .min(1_000_000_000);
        let gap = DECREASE_GAP_NS.max(rtt.saturating_mul(2));
        // Queueing must persist over two ticks: a single IDR crossing the link
        // queues briefly and is not a link change. Loss acts at once.
        let over = queueing > interval;
        let persistent = over && self.queued;
        self.queued = over;
        if persistent || loss {
            let due = self
                .last_decrease_ns
                .is_none_or(|last| now.saturating_sub(last) >= gap);
            if due {
                self.decrease(now, video_bps);
            } else {
                self.state = AbrState::Hold;
            }
        } else if over || queueing > interval / 2 || growing {
            self.state = AbrState::Hold;
        } else if (video_bps as f64) < self.target * APP_LIMITED {
            self.state = AbrState::AppLimited;
        } else {
            self.state = AbrState::Increase;
            let factor = 1.0 + INCREASE_PER_SECOND * elapsed as f64 / 1e9;
            self.target = (self.target * factor).min(self.cap(self.fps_index, self.scale_index));
            self.step_up(now);
        }
        let floor = self.floor_abs();
        self.target = self.target.clamp(
            floor.min(self.cap(self.fps_index, self.scale_index)),
            self.cap(self.fps_index, self.scale_index),
        );
        let after = self.target();
        (after != before).then_some(after)
    }

    fn decrease(&mut self, now: u64, video_bps: u64) {
        self.state = AbrState::Decrease;
        self.decreases += 1;
        self.last_decrease_ns = Some(now);
        let base = if video_bps > 0 {
            self.target.min(video_bps as f64)
        } else {
            self.target
        };
        self.target = (base * DECREASE).min(self.target);
        if let Some(bulk) = self.bulk_floor
            && self.target <= bulk
        {
            // Pinned at the bulk floor: no tier step either.
            self.target = bulk;
            return;
        }
        self.target = self.target.max(MIN_TARGET_BPS as f64);
        // Below this tier's quality floor: the frame rate, then the resolution.
        while self.target < self.floor(self.fps_index, self.scale_index) {
            if self.fps_index + 1 < self.fps_levels.len() {
                self.fps_index += 1;
            } else if self.config.resolution_steps && self.scale_index + 1 < SCALE_LEVELS.len() {
                self.scale_index += 1;
            } else {
                break;
            }
            self.tier_changes += 1;
            self.last_step_ns = now;
        }
    }

    /// Back up the ladder in reverse: the resolution first (it went down
    /// last), then the frame rate, each after a quiet period and only once the
    /// target has room for the higher tier.
    fn step_up(&mut self, now: u64) {
        let quiet_since = self.last_decrease_ns.unwrap_or(0).max(self.last_step_ns);
        let quiet = now.saturating_sub(quiet_since);
        let room = |this: &Self, fps_index: usize, scale_index: usize| {
            let need = (this.floor(fps_index, scale_index) * 2.0)
                .min(this.cap(this.fps_index, this.scale_index));
            this.target >= need
        };
        if self.scale_index > 0 {
            if quiet >= RESOLUTION_UP_QUIET_NS && room(self, self.fps_index, self.scale_index - 1) {
                self.scale_index -= 1;
                self.tier_changes += 1;
                self.last_step_ns = now;
            }
        } else if self.fps_index > 0
            && quiet >= FPS_UP_QUIET_NS
            && room(self, self.fps_index - 1, self.scale_index)
        {
            self.fps_index -= 1;
            self.tier_changes += 1;
            self.last_step_ns = now;
        }
    }

    fn size(&self, scale_index: usize) -> (u32, u32) {
        let (num, den) = SCALE_LEVELS[scale_index];
        let scale =
            |edge: u32| ((u64::from(edge) * u64::from(num) / u64::from(den)) as u32 & !1).max(2);
        if scale_index == 0 {
            (self.config.width, self.config.height)
        } else {
            (scale(self.config.width), scale(self.config.height))
        }
    }

    fn pixel_rate(&self, fps_index: usize, scale_index: usize) -> f64 {
        let (w, h) = self.size(scale_index);
        f64::from(w) * f64::from(h) * f64::from(self.fps_levels[fps_index])
    }

    /// A tier's ceiling: the plan's bitrate scaled by the tier's pixel rate,
    /// clamped to 2–80 Mbit/s and the medium cap.
    fn cap(&self, fps_index: usize, scale_index: usize) -> f64 {
        let full = self.pixel_rate(0, 0).max(1.0);
        let scaled =
            self.config.bitrate_bps as f64 * self.pixel_rate(fps_index, scale_index) / full;
        let max = self
            .config
            .medium_cap_bps
            .map_or(MAX_BITRATE_BPS, |cap| cap.min(MAX_BITRATE_BPS)) as f64;
        scaled.min(max).max(MIN_BITRATE_BPS as f64)
    }

    /// A tier's quality floor, `w·h·fps·0.02`, at most its cap.
    fn floor(&self, fps_index: usize, scale_index: usize) -> f64 {
        (self.pixel_rate(fps_index, scale_index) * FLOOR_BITS_PER_PIXEL)
            .min(self.cap(fps_index, scale_index))
    }

    /// The lowest the target goes: [`MIN_TARGET_BPS`], or the bulk floor.
    fn floor_abs(&self) -> f64 {
        (MIN_TARGET_BPS as f64).max(self.bulk_floor.unwrap_or(0.0))
    }

    /// Forget delay history (a transport change: new path, new minima).
    pub fn reset_delay(&mut self) {
        self.credit_min.clear();
        self.link_min.clear();
        self.client_min.clear();
        self.client_points.clear();
        self.client_excess = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;
    const MBPS: u64 = 1_000_000;

    fn hd() -> AbrConfig {
        // 1920 × 1080 × 60 at q = 0.5: 13.7 Mbit/s.
        AbrConfig {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 13_700_000,
            medium_cap_bps: None,
            resolution_steps: true,
        }
    }

    /// A steady sample at `now` with the encoder producing `video_bps`.
    fn idle(now: u64, video_bps: u64) -> AbrSample {
        AbrSample {
            now_ns: now,
            credit_rtt_ns: Some(2 * MS),
            video_bps,
            ..AbrSample::default()
        }
    }

    #[test]
    fn starts_at_the_plan_bitrate_and_holds_on_a_steady_link() {
        let mut abr = AbrController::new(hd());
        assert_eq!(abr.target().bitrate_bps, 13_700_000);
        let mut now = 0;
        for _ in 0..600 {
            now += 100 * MS;
            let changed = abr.tick(&idle(now, 13 * MBPS));
            assert_eq!(changed, None, "no change on a steady link with room");
        }
        assert_eq!(abr.decreases(), 0);
        assert_eq!(abr.tier_changes(), 0);
    }

    /// Ticks a steady link for 1.2 s (past the rate warm-up); returns the time.
    fn warm(abr: &mut AbrController, video_bps: u64) -> u64 {
        let mut now = 0;
        for _ in 0..12 {
            now += 100 * MS;
            abr.tick(&idle(now, video_bps));
        }
        now
    }

    /// Two ticks 100 ms apart that both see `sample`'s queue (it must
    /// persist); returns the time of the second.
    fn queued(abr: &mut AbrController, mut sample: AbrSample) -> u64 {
        abr.tick(&sample);
        assert_ne!(abr.state(), AbrState::Decrease, "one tick is not enough");
        sample.now_ns += 100 * MS;
        abr.tick(&sample);
        sample.now_ns
    }

    #[test]
    fn queueing_above_one_frame_multiplies_by_0_85() {
        let mut abr = AbrController::new(hd());
        let now = warm(&mut abr, 13_700_000);
        let before = abr.target().bitrate_bps as f64;
        let mut sample = idle(now + 100 * MS, 13_700_000);
        sample.unsent_age_ns = 17 * MS; // one 60 fps frame is 16.7 ms
        queued(&mut abr, sample);
        assert_eq!(abr.state(), AbrState::Decrease);
        let after = abr.target().bitrate_bps as f64;
        assert!((after / before - 0.85).abs() < 0.001, "{after} / {before}");
        // A queue of less than one frame does not decrease.
        let mut abr = AbrController::new(hd());
        let now = warm(&mut abr, 13_700_000);
        let mut sample = idle(now + 100 * MS, 13_700_000);
        sample.unsent_age_ns = 16 * MS;
        abr.tick(&sample);
        sample.now_ns += 100 * MS;
        abr.tick(&sample);
        assert_ne!(abr.state(), AbrState::Decrease);
        assert_eq!(abr.target().bitrate_bps, 13_700_000);
    }

    #[test]
    fn a_single_queued_tick_is_not_a_link_change() {
        // An IDR crossing the link: one tick sees a queue, the next does not.
        let mut abr = AbrController::new(hd());
        let now = warm(&mut abr, 13 * MBPS);
        let mut sample = idle(now + 100 * MS, 13 * MBPS);
        sample.unsent_age_ns = 40 * MS;
        abr.tick(&sample);
        assert_eq!(abr.state(), AbrState::Hold);
        abr.tick(&idle(now + 200 * MS, 13 * MBPS));
        assert_eq!(abr.decreases(), 0);
    }

    #[test]
    fn decreases_follow_the_measured_rate_and_keep_a_gap() {
        let mut abr = AbrController::new(hd());
        let now = warm(&mut abr, 13 * MBPS);
        let mut sample = idle(now + 100 * MS, 8 * MBPS);
        sample.unsent_age_ns = 40 * MS;
        let now = queued(&mut abr, sample);
        assert_eq!(
            abr.target().bitrate_bps,
            6_800_000,
            "0.85 × the measured 8 Mbit/s"
        );
        // 100 ms later the queue is still there: within the gap it holds.
        sample.now_ns = now + 100 * MS;
        abr.tick(&sample);
        assert_eq!(abr.state(), AbrState::Hold);
        assert_eq!(abr.decreases(), 1);
        sample.now_ns = now + 300 * MS;
        abr.tick(&sample);
        assert_eq!(abr.decreases(), 2);
    }

    #[test]
    fn the_measured_rate_counts_only_after_its_window() {
        // At start the one-second window is nearly empty: the decrease takes
        // the target, not a tenth of the rate.
        let mut abr = AbrController::new(hd());
        let mut sample = idle(100 * MS, MBPS);
        sample.unsent_age_ns = 40 * MS;
        queued(&mut abr, sample);
        assert_eq!(abr.target().bitrate_bps, 11_645_000);
    }

    #[test]
    fn ramps_by_six_percent_per_second() {
        let mut config = hd();
        config.bitrate_bps = 80 * MBPS;
        let mut abr = AbrController::new(config);
        let now = warm(&mut abr, 80 * MBPS);
        // Bring it down first.
        let mut sample = idle(now + 100 * MS, 10 * MBPS);
        sample.unsent_age_ns = 40 * MS;
        let mut now = queued(&mut abr, sample);
        let start = abr.target().bitrate_bps as f64;
        for _ in 0..10 {
            now += 100 * MS;
            abr.tick(&idle(now, abr.target().bitrate_bps));
        }
        let after = abr.target().bitrate_bps as f64;
        let rate = after / start - 1.0;
        assert!((0.05..=0.08).contains(&rate), "{rate} per second");
    }

    #[test]
    fn an_application_limited_encoder_does_not_ramp() {
        let mut abr = AbrController::new(hd());
        let now = warm(&mut abr, 13 * MBPS);
        let mut sample = idle(now + 100 * MS, 10 * MBPS);
        sample.unsent_age_ns = 40 * MS;
        let mut now = queued(&mut abr, sample);
        let low = abr.target().bitrate_bps;
        for _ in 0..50 {
            now += 100 * MS;
            abr.tick(&idle(now, MBPS));
        }
        assert_eq!(abr.state(), AbrState::AppLimited);
        assert_eq!(abr.target().bitrate_bps, low);
    }

    #[test]
    fn credit_rtt_above_its_minimum_is_queueing() {
        let mut abr = AbrController::new(hd());
        let now = warm(&mut abr, 13 * MBPS);
        let mut sample = idle(now + 100 * MS, 13 * MBPS);
        // 2 ms is the minimum; 30 ms is 28 ms of queue, more than a frame.
        sample.credit_rtt_ns = Some(30 * MS);
        queued(&mut abr, sample);
        assert_eq!(abr.state(), AbrState::Decrease);
        // A high RTT from the start is no queue: it is the minimum.
        let mut abr = AbrController::new(hd());
        let mut sample = idle(100 * MS, 13 * MBPS);
        sample.credit_rtt_ns = Some(30 * MS);
        abr.tick(&sample);
        sample.now_ns += 100 * MS;
        abr.tick(&sample);
        assert_ne!(abr.state(), AbrState::Decrease);
    }

    #[test]
    fn the_client_delay_gradient_holds_and_its_excess_decreases() {
        let mut abr = AbrController::new(hd());
        // A clock offset of 5 s: only the excess over the minimum counts.
        let offset = 5_000_000_000;
        let frames = |start: u64, delay_us: &dyn Fn(u64) -> u64| -> Vec<(u64, u64)> {
            (0..6)
                .map(|i| {
                    let h5 = start + i * 16_667;
                    (h5, h5 + offset + delay_us(i))
                })
                .collect()
        };
        let now = warm(&mut abr, 13 * MBPS);
        abr.note_client_delay(now, &frames(100_000, &|_| 3_000));
        abr.tick(&idle(now + 100 * MS, 13 * MBPS));
        assert_eq!(abr.state(), AbrState::Increase);
        // Rising by 1.5 ms per frame (0.09 µs/µs) but still below half a frame
        // of excess: hold.
        abr.note_client_delay(now + 200 * MS, &frames(200_000, &|i| 3_000 + 1_500 * i));
        abr.tick(&idle(now + 200 * MS, 13 * MBPS));
        assert!(abr.client_gradient().unwrap() > GRADIENT_HOLD);
        assert_eq!(abr.state(), AbrState::Hold);
        // 25 ms over the minimum, in two reports: decrease.
        abr.note_client_delay(now + 300 * MS, &frames(300_000, &|_| 28_000));
        abr.tick(&idle(now + 300 * MS, 13 * MBPS));
        abr.note_client_delay(now + 400 * MS, &frames(400_000, &|_| 28_000));
        abr.tick(&idle(now + 400 * MS, 13 * MBPS));
        assert_eq!(abr.state(), AbrState::Decrease);
        // A stale report counts no more.
        let mut abr2 = abr.clone();
        abr2.tick(&idle(now + 2_000 * MS, 13 * MBPS));
        abr2.tick(&idle(now + 2_100 * MS, 13 * MBPS));
        assert_ne!(abr2.state(), AbrState::Decrease);
    }

    #[test]
    fn one_large_frame_is_not_a_queue() {
        let mut abr = AbrController::new(hd());
        let mut samples: Vec<(u64, u64)> =
            (0..6).map(|i| (i * 16_667, i * 16_667 + 2_000)).collect();
        // An IDR that took 30 ms to arrive.
        samples[3].1 += 30_000;
        abr.note_client_delay(100 * MS, &samples);
        abr.tick(&idle(100 * MS, 13 * MBPS));
        abr.tick(&idle(200 * MS, 13 * MBPS));
        assert_ne!(abr.state(), AbrState::Decrease);
    }

    #[test]
    fn loss_decreases_at_once() {
        let mut abr = AbrController::new(hd());
        let now = warm(&mut abr, 13 * MBPS);
        let mut sample = idle(now + 100 * MS, 13 * MBPS);
        sample.lost_frames = 1;
        abr.tick(&sample);
        assert_eq!(abr.state(), AbrState::Decrease);
        // The same count again is no new loss.
        sample.now_ns += 400 * MS;
        abr.tick(&sample);
        assert_ne!(abr.state(), AbrState::Decrease);
        // QUIC loss, and the QUIC RTT above its minimum.
        sample.now_ns += 400 * MS;
        sample.link_lost_packets = 3;
        abr.tick(&sample);
        assert_eq!(abr.state(), AbrState::Decrease);
        sample.now_ns += 400 * MS;
        sample.link_rtt_ns = Some(5 * MS);
        abr.tick(&sample);
        sample.now_ns += 400 * MS;
        sample.link_rtt_ns = Some(40 * MS);
        queued(&mut abr, sample);
        assert_eq!(abr.state(), AbrState::Decrease);
    }

    #[test]
    fn the_ladder_is_bitrate_then_fps_then_resolution() {
        let mut abr = AbrController::new(hd());
        let mut now = 0;
        let mut seen = vec![abr.target()];
        // A link of about 1.5 Mbit/s: far below every tier.
        for _ in 0..400 {
            now += 300 * MS;
            let mut sample = idle(now, abr.target().bitrate_bps);
            sample.unsent_age_ns = 100 * MS;
            if let Some(target) = abr.tick(&sample) {
                seen.push(target);
            }
        }
        // The bitrate went down first, at full fps and size, to the 60 fps
        // floor (1920 × 1080 × 60 × 0.02 = 2.49 Mbit/s).
        let first_fps_step = seen.iter().position(|t| t.fps < 60).unwrap();
        assert!(seen[..first_fps_step].iter().all(|t| t.width == 1920));
        assert!(seen[first_fps_step - 1].bitrate_bps < 2_900_000);
        // Then fps steps, all at full size, down to the minimum.
        let first_size_step = seen.iter().position(|t| t.width < 1920).unwrap();
        assert!(first_size_step > first_fps_step);
        assert_eq!(seen[first_size_step].fps, MIN_FPS);
        let fps: Vec<u32> = seen[..first_size_step].iter().map(|t| t.fps).collect();
        assert!(
            fps.windows(2).all(|w| w[1] <= w[0]),
            "fps only went down: {fps:?}"
        );
        // Then resolution, to half size, and the bitrate to its minimum.
        let last = abr.target();
        assert_eq!((last.width, last.height, last.fps), (960, 540, MIN_FPS));
        assert!(seen.iter().all(|t| t.bitrate_bps >= MIN_TARGET_BPS));
        assert_eq!(last.bitrate_bps, MIN_TARGET_BPS);
    }

    #[test]
    fn steps_back_up_in_reverse_after_a_quiet_period() {
        let mut abr = AbrController::new(hd());
        let mut now = 0;
        for _ in 0..400 {
            now += 300 * MS;
            let mut sample = idle(now, abr.target().bitrate_bps);
            sample.unsent_age_ns = 100 * MS;
            abr.tick(&sample);
        }
        assert_eq!(abr.target().width, 960);
        let mut order = Vec::new();
        for _ in 0..3_000 {
            now += 100 * MS;
            let before = abr.target();
            if let Some(after) = abr.tick(&idle(now, abr.target().bitrate_bps)) {
                if after.width != before.width {
                    order.push("size");
                }
                if after.fps != before.fps {
                    order.push("fps");
                }
            }
        }
        let first_fps = order.iter().position(|s| *s == "fps").unwrap();
        assert!(order[..first_fps].iter().all(|s| *s == "size"), "{order:?}");
        assert_eq!(
            abr.target(),
            AbrTarget {
                bitrate_bps: 13_700_000,
                fps: 60,
                width: 1920,
                height: 1080
            }
        );
    }

    #[test]
    fn without_resolution_steps_the_ladder_stops_at_the_lowest_fps() {
        let mut config = hd();
        config.resolution_steps = false;
        let mut abr = AbrController::new(config);
        let mut now = 0;
        for _ in 0..400 {
            now += 300 * MS;
            let mut sample = idle(now, abr.target().bitrate_bps);
            sample.unsent_age_ns = 100 * MS;
            abr.tick(&sample);
        }
        assert_eq!((abr.target().width, abr.target().fps), (1920, MIN_FPS));
    }

    #[test]
    fn clamps_to_2_to_80_mbps_and_the_medium_cap() {
        let mut config = hd();
        config.bitrate_bps = 200 * MBPS;
        assert_eq!(AbrController::new(config).target().bitrate_bps, 80 * MBPS);
        config.medium_cap_bps = Some(30 * MBPS);
        assert_eq!(AbrController::new(config).target().bitrate_bps, 30 * MBPS);
        config.bitrate_bps = 500_000;
        config.medium_cap_bps = None;
        assert_eq!(
            AbrController::new(config).target().bitrate_bps,
            MIN_BITRATE_BPS
        );
        // Ramping never passes the cap.
        let mut config = hd();
        config.medium_cap_bps = Some(10 * MBPS);
        let mut abr = AbrController::new(config);
        let mut now = 0;
        for _ in 0..100 {
            now += 100 * MS;
            abr.tick(&idle(now, 10 * MBPS));
        }
        assert_eq!(abr.target().bitrate_bps, 10 * MBPS);
    }

    #[test]
    fn bulk_competition_keeps_a_floor() {
        let mut abr = AbrController::new(hd());
        let mut now = 0;
        for _ in 0..200 {
            now += 300 * MS;
            let mut sample = idle(now, abr.target().bitrate_bps);
            sample.unsent_age_ns = 100 * MS;
            sample.bulk_active = true;
            abr.tick(&sample);
        }
        let target = abr.target();
        assert_eq!(
            target.bitrate_bps,
            13_700_000 / 2,
            "half the rate before bulk"
        );
        assert_eq!(
            (target.fps, target.width),
            (60, 1920),
            "no tier step at the floor"
        );
        // With a known capacity the floor is at least 40 % of it.
        let mut abr = AbrController::new(hd());
        let mut sample = idle(100 * MS, 13 * MBPS);
        sample.bulk_active = true;
        sample.capacity_bps = Some(20 * MBPS);
        sample.unsent_age_ns = 100 * MS;
        for _ in 0..50 {
            sample.now_ns += 300 * MS;
            abr.tick(&sample);
        }
        assert_eq!(abr.target().bitrate_bps, 8 * MBPS);
        // Once bulk ends, the floor goes.
        sample.bulk_active = false;
        for _ in 0..50 {
            sample.now_ns += 300 * MS;
            abr.tick(&sample);
        }
        assert!(abr.target().bitrate_bps < 8 * MBPS);
    }

    #[test]
    fn a_sawtooth_on_a_steady_constrained_link_keeps_its_tier() {
        // A link of 8 Mbit/s: the queue grows only while the target exceeds it.
        let mut abr = AbrController::new(hd());
        let capacity = 8.0 * MBPS as f64;
        let mut queue_bits = 0.0f64;
        let mut now = 0;
        let mut tiers = 0;
        let mut low = u64::MAX;
        let mut high = 0;
        for tick in 0..3_000 {
            now += 100 * MS;
            let rate = abr.target().bitrate_bps as f64;
            queue_bits = (queue_bits + (rate - capacity) * 0.1).max(0.0);
            let mut sample = idle(now, rate as u64);
            sample.unsent_age_ns = (queue_bits / capacity * 1e9) as u64;
            if let Some(t) = abr.tick(&sample)
                && (t.fps != 60 || t.width != 1920)
            {
                tiers += 1;
            }
            if tick > 600 {
                low = low.min(abr.target().bitrate_bps);
                high = high.max(abr.target().bitrate_bps);
            }
        }
        assert_eq!(tiers, 0, "no fps or resolution flapping");
        assert!(low as f64 >= capacity * 0.6, "low {low}");
        assert!(high as f64 <= capacity * 1.15, "high {high}");
    }
}
