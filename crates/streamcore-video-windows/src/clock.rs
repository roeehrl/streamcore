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

/// Nanoseconds to milliseconds as a float, for reports.
pub fn ns_to_ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
