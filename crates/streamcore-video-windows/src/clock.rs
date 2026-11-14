//! Host-monotonic time. Every stamp in the spike is nanoseconds on the QPC clock, the clock that
//! `Direct3D11CaptureFrame.SystemRelativeTime` and DXGI's `LastPresentTime` use (design doc 9.2).

/// Converts a QPC tick count to nanoseconds. Uses 128-bit maths so neither a large tick count
/// nor an unusual frequency overflows or loses precision.
pub fn qpc_to_ns(ticks: u64, frequency: u64) -> u64 {
    if frequency == 0 {
        return 0;
    }
    let ns = u128::from(ticks) * 1_000_000_000 / u128::from(frequency);
    u64::try_from(ns).unwrap_or(u64::MAX)
}

/// Converts a WinRT `TimeSpan` (100 ns units, QPC based for `SystemRelativeTime`) to nanoseconds.
/// Negative values clamp to zero.
pub fn hundred_ns_to_ns(value: i64) -> u64 {
    u64::try_from(value).map_or(0, |v| v.saturating_mul(100))
}

/// Converts nanoseconds to 100 ns units (Media Foundation sample times).
pub fn ns_to_hundred_ns(ns: u64) -> i64 {
    i64::try_from(ns / 100).unwrap_or(i64::MAX)
}

/// A refresh interval in ns from a display rate given as a fraction (numerator / denominator Hz,
/// as `QueryDisplayConfig` reports it): 1 s × `den` / `num`, rounded. `None` for a zero rate.
pub fn interval_from_rational(num: u32, den: u32) -> Option<u64> {
    if num == 0 || den == 0 {
        return None;
    }
    let ns = (u128::from(den) * 1_000_000_000 + u128::from(num) / 2) / u128::from(num);
    u64::try_from(ns).ok().filter(|ns| *ns > 0)
}

/// Nanoseconds to milliseconds as a float, for reports.
pub fn ns_to_ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

/// A process power-throttling state, `(ControlMask, StateMask)`, with only `bit` changed:
/// `restore: None` opts the process out of that throttling (set in the control mask, cleared in
/// the state mask: "always off"), `Some(saved)` puts `bit` back as it was in `saved`. Every other
/// policy (EcoQoS's execution speed, anything a host set) is left as `current` has it.
pub fn throttling_with(current: (u32, u32), bit: u32, restore: Option<(u32, u32)>) -> (u32, u32) {
    let (control, state) = current;
    match restore {
        None => (control | bit, state & !bit),
        Some((saved_control, saved_state)) => (
            (control & !bit) | (saved_control & bit),
            (state & !bit) | (saved_state & bit),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttling_touches_only_its_bit() {
        const SPEED: u32 = 0x1; // EXECUTION_SPEED (EcoQoS)
        const TIMER: u32 = 0x4; // IGNORE_TIMER_RESOLUTION
        // A host that turned EcoQoS on: opting out of timer throttling keeps it.
        let eco = (SPEED, SPEED);
        let held = throttling_with(eco, TIMER, None);
        assert_eq!(held, (SPEED | TIMER, SPEED));
        assert_eq!(throttling_with(held, TIMER, Some(eco)), eco);
        // A host that had asked for timer throttling explicitly gets it back.
        let throttled = (TIMER, TIMER);
        let held = throttling_with(throttled, TIMER, None);
        assert_eq!(held, (TIMER, 0));
        assert_eq!(throttling_with(held, TIMER, Some(throttled)), throttled);
        // A change the host made to another policy while held survives the restore.
        let changed = (held.0 | SPEED, held.1 | SPEED);
        assert_eq!(
            throttling_with(changed, TIMER, Some(throttled)),
            (TIMER | SPEED, TIMER | SPEED)
        );
        // The default (system decides) round-trips to the default.
        assert_eq!(
            throttling_with(throttling_with((0, 0), TIMER, None), TIMER, Some((0, 0))),
            (0, 0)
        );
    }

    #[test]
    fn ten_megahertz_is_exact() {
        assert_eq!(qpc_to_ns(1, 10_000_000), 100);
        assert_eq!(qpc_to_ns(10_000_000, 10_000_000), 1_000_000_000);
    }

    #[test]
    fn odd_frequency_does_not_overflow() {
        // A 3.x MHz TSC-derived QPC and a tick count from months of uptime.
        let freq = 3_579_545;
        let ticks = 30_000_000_000_000u64;
        let ns = qpc_to_ns(ticks, freq);
        let expected = (u128::from(ticks) * 1_000_000_000 / u128::from(freq)) as u64;
        assert_eq!(ns, expected);
        assert!(ns > ticks); // would have overflowed in u64 maths
    }

    #[test]
    fn zero_frequency_is_zero() {
        assert_eq!(qpc_to_ns(5, 0), 0);
    }

    #[test]
    fn timespan_round_trip() {
        assert_eq!(hundred_ns_to_ns(166_667), 16_666_700);
        assert_eq!(hundred_ns_to_ns(-5), 0);
        assert_eq!(ns_to_hundred_ns(16_666_700), 166_667);
        assert!((ns_to_ms(1_500_000) - 1.5).abs() < 1e-12);
    }

    #[test]
    fn refresh_interval_from_a_rational_rate() {
        assert_eq!(interval_from_rational(60, 1), Some(16_666_667));
        assert_eq!(interval_from_rational(60_000, 1001), Some(16_683_333));
        assert_eq!(interval_from_rational(144_000, 1000), Some(6_944_444));
        assert_eq!(interval_from_rational(0, 1), None);
        assert_eq!(interval_from_rational(60, 0), None);
    }
}
