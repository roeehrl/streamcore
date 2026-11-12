//! The P2 pacer's pure parts (STREAMCORE-VIDEO.md 6.2, 6.3): refresh-aligned
//! admission and the adaptive encoder permit target. The executor owns one of
//! each per stream; both are plain state machines on host times, so they are
//! tested without an executor.

use std::collections::VecDeque;

/// The encoder permit hard cap (6.2), `streamcore_media::stream::MAX_IN_FLIGHT`.
pub const MAX_PERMITS: u8 = 3;

/// Refresh-aligned admission (6.3).
///
/// The due time advances by `target_ns` from the previous due time, never from
/// the last admitted H0, and is resynchronised after idle. A frame is admitted
/// when `H0 >= due - refresh_ns / 2`; an earlier one only updates the latest
/// slot. On a 144 Hz panel a 60 fps target then admits 60 frames per second,
/// each within half a refresh of its due time; measuring from the last admitted
/// H0 would admit every second refresh (72 fps).
#[derive(Debug, Clone)]
pub struct RefreshPacer {
    target_ns: u64,
    refresh_ns: u64,
    due: Option<u64>,
}

impl RefreshPacer {
    /// `refresh_ns` 0 uses `target_ns` (the fallback when no refresh interval
    /// is known).
    pub fn new(target_ns: u64, refresh_ns: u64) -> Self {
        let target_ns = target_ns.max(1);
        Self {
            target_ns,
            refresh_ns: if refresh_ns == 0 {
                target_ns
            } else {
                refresh_ns
            },
            due: None,
        }
    }

    /// The target interval, `1 s / fps`.
    pub fn target_ns(&self) -> u64 {
        self.target_ns
    }

    /// The refresh interval admission aligns to.
    pub fn refresh_ns(&self) -> u64 {
        self.refresh_ns
    }

    /// The next due time (host clock), once a frame was admitted.
    pub fn due_ns(&self) -> Option<u64> {
        self.due
    }

    /// A new target rate; the due time keeps its phase.
    pub fn set_target(&mut self, target_ns: u64) {
        let fallback = self.refresh_ns == self.target_ns;
        self.target_ns = target_ns.max(1);
        if fallback {
            self.refresh_ns = self.target_ns;
        }
    }

    /// A new refresh interval (a display mode change); 0 uses the target.
    pub fn set_refresh(&mut self, refresh_ns: u64) {
        self.refresh_ns = if refresh_ns == 0 {
            self.target_ns
        } else {
            refresh_ns
        };
    }

    /// Forget the due time: the next frame is admitted and restarts the cadence.
    pub fn reset(&mut self) {
        self.due = None;
    }

    /// Whether a frame presented at `h0` is due. An admitted frame advances
    /// the due time.
    pub fn offer(&mut self, h0: u64) -> bool {
        let Some(due) = self.due else {
            self.due = Some(h0.saturating_add(self.target_ns));
            return true;
        };
        // Idle for more than an interval past the due time: start again here.
        if h0 > due.saturating_add(self.target_ns) {
            self.due = Some(h0.saturating_add(self.target_ns));
            return true;
        }
        if h0.saturating_add(self.refresh_ns / 2) < due {
            return false;
        }
        self.due = Some(due.saturating_add(self.target_ns));
        true
    }

    /// A held frame becomes due at host time `now` because nothing newer came:
    /// admit it as a frame presented at `max(now, due)`.
    pub fn admit_held(&mut self, now: u64) {
        let at = self.due.map_or(now, |due| now.max(due));
        let admitted = self.offer(at);
        debug_assert!(admitted);
    }

    /// How long after a held frame's arrival its timer fires: half a refresh
    /// (plus a little) past the due time, so a frame presented on time still
    /// arrives first, at most one target interval plus one refresh.
    pub fn hold_delay_ns(&self, h0: u64) -> u64 {
        let due = self.due.unwrap_or(h0);
        let until_due = due.saturating_sub(h0);
        until_due
            .saturating_add(self.refresh_ns / 2)
            .saturating_add(HOLD_GRACE_NS)
            .min(self.target_ns.saturating_add(self.refresh_ns))
    }
}

const HOLD_GRACE_NS: u64 = 1_000_000;

/// The adaptive encoder permit target (6.2): the plan's base
/// (`max(min_inputs_before_output, 1)`), plus one while the encode time p95
/// exceeds 0.8 x the frame interval, never above [`MAX_PERMITS`]. The extra
/// permit is withdrawn once the p95 falls to 0.6 x the interval (hysteresis).
#[derive(Debug, Clone)]
pub struct PermitGovernor {
    base: u8,
    boosted: bool,
    samples: VecDeque<u64>,
}

/// Encode times kept for the p95.
pub const PERMIT_WINDOW: usize = 32;
/// Samples needed before the target changes.
pub const PERMIT_MIN_SAMPLES: usize = 8;

impl PermitGovernor {
    pub fn new(base: u8) -> Self {
        Self {
            base: base.clamp(1, MAX_PERMITS),
            boosted: false,
            samples: VecDeque::with_capacity(PERMIT_WINDOW),
        }
    }

    pub fn target(&self) -> u8 {
        (self.base + u8::from(self.boosted)).min(MAX_PERMITS)
    }

    /// Encode time p95 over the window, once there are enough samples.
    pub fn p95_ns(&self) -> Option<u64> {
        if self.samples.len() < PERMIT_MIN_SAMPLES {
            return None;
        }
        let mut sorted: Vec<u64> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        let rank = (sorted.len() * 95).div_ceil(100).max(1) - 1;
        Some(sorted[rank])
    }

    /// One frame's encode time at a frame interval of `interval_ns`.
    pub fn record(&mut self, encode_ns: u64, interval_ns: u64) {
        if self.samples.len() == PERMIT_WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(encode_ns);
        let Some(p95) = self.p95_ns() else {
            return;
        };
        // p95 > 0.8 x interval adds a permit; <= 0.6 x interval removes it.
        if !self.boosted && p95.saturating_mul(10) > interval_ns.saturating_mul(8) {
            self.boosted = true;
        } else if self.boosted && p95.saturating_mul(10) <= interval_ns.saturating_mul(6) {
            self.boosted = false;
        }
    }

    /// A new encoder session: its encode times start over.
    pub fn reset(&mut self) {
        self.boosted = false;
        self.samples.clear();
    }
}
