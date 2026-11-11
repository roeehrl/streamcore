//! Nearest-rank percentiles over per-frame stage durations.

use serde::{Deserialize, Serialize};

/// Summary of one stage over a run, in milliseconds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub n: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
}

/// Nearest-rank percentile (`p` in 0..=100) of sorted values. `None` for an empty slice.
pub fn percentile_sorted(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let p = p.clamp(0.0, 100.0);
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    let index = rank.max(1) - 1;
    sorted.get(index.min(sorted.len() - 1)).copied()
}

/// Summarises durations in nanoseconds.
pub fn summarize_ns(values: &[u64]) -> Summary {
    if values.is_empty() {
        return Summary::default();
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let ms = |v: Option<u64>| v.map_or(0.0, crate::clock::ns_to_ms);
    let sum: u128 = sorted.iter().map(|v| u128::from(*v)).sum();
    Summary {
        n: sorted.len(),
        p50_ms: ms(percentile_sorted(&sorted, 50.0)),
        p95_ms: ms(percentile_sorted(&sorted, 95.0)),
        p99_ms: ms(percentile_sorted(&sorted, 99.0)),
        max_ms: ms(sorted.last().copied()),
        mean_ms: (sum as f64 / sorted.len() as f64) / 1_000_000.0,
    }
}

/// Signed variant for stages whose order is not guaranteed (for example a fence observed before
/// a CPU stamp on a slow wake). Negative values clamp to zero and are counted.
pub fn summarize_signed_ns(values: &[i64]) -> (Summary, usize) {
    let negatives = values.iter().filter(|v| **v < 0).count();
    let clamped: Vec<u64> = values
        .iter()
        .map(|v| u64::try_from(*v).unwrap_or(0))
        .collect();
    (summarize_ns(&clamped), negatives)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile_sorted(&v, 50.0), Some(50));
        assert_eq!(percentile_sorted(&v, 95.0), Some(95));
        assert_eq!(percentile_sorted(&v, 99.0), Some(99));
        assert_eq!(percentile_sorted(&v, 100.0), Some(100));
        assert_eq!(percentile_sorted(&v, 0.0), Some(1));
    }

    #[test]
    fn small_and_empty() {
        assert_eq!(percentile_sorted(&[], 50.0), None);
        assert_eq!(percentile_sorted(&[7], 99.0), Some(7));
        assert_eq!(percentile_sorted(&[1, 2], 50.0), Some(1));
        assert_eq!(summarize_ns(&[]), Summary::default());
    }

    #[test]
    fn summary_in_ms() {
        let s = summarize_ns(&[3_000_000, 1_000_000, 2_000_000]);
        assert_eq!(s.n, 3);
        assert!((s.p50_ms - 2.0).abs() < 1e-9);
        assert!((s.max_ms - 3.0).abs() < 1e-9);
        assert!((s.mean_ms - 2.0).abs() < 1e-9);
    }

    #[test]
    fn signed_clamps() {
        let (s, neg) = summarize_signed_ns(&[-5, 1_000_000]);
        assert_eq!(neg, 1);
        assert_eq!(s.n, 2);
        assert!((s.max_ms - 1.0).abs() < 1e-9);
    }
}
