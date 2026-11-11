//! Adapter policy (design doc 4.2) as a pure function over the adapter report.
//!
//! Hybrid options as the spike names them (the doc says only "hybrid spike"):
//! - **C**: encode on the adapter that drives the output; no cross-adapter copy.
//! - **A**: capture with WGC on the encoder adapter; DWM copies each composed frame across.
//! - **B**: capture and convert on the display adapter, copy NV12 across (D3D12 cross-adapter).
//!   Not implemented in the spike.
//!
//! Order: C if the display adapter has a hardware H.264 MFT, else A on a discrete adapter with
//! one, else A on any other hardware adapter with one, else the software MFT (≤ 1080p30).

use crate::model::{AdapterInfo, AdapterReport};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HybridOption {
    C,
    A,
    Software,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub monitor_index: u32,
    pub option: HybridOption,
    /// Adapter (LUID) WGC captures on.
    pub capture_adapter: String,
    /// Adapter (LUID) that encodes; empty for the software MFT.
    pub encode_adapter: String,
    pub reason: String,
}

fn has_h264(a: &AdapterInfo) -> bool {
    !a.software && a.encoders.iter().any(|e| e.hardware && e.codec == "h264")
}

/// Treat an adapter as discrete when it has at least 1 GiB of dedicated memory.
fn is_discrete(a: &AdapterInfo) -> bool {
    !a.software && a.dedicated_video_memory_mb >= 1024
}

/// The choice for one output, or `None` if no adapter drives it.
pub fn choose(report: &AdapterReport, monitor_index: u32) -> Option<Choice> {
    let display = report
        .adapters
        .iter()
        .find(|a| a.outputs.iter().any(|o| o.monitor_index == monitor_index))?;
    let make = |option, encode: &str, reason: &str| Choice {
        monitor_index,
        option,
        capture_adapter: if encode.is_empty() {
            display.luid.clone()
        } else {
            encode.to_string()
        },
        encode_adapter: encode.to_string(),
        reason: reason.to_string(),
    };
    if has_h264(display) {
        return Some(make(
            HybridOption::C,
            &display.luid,
            "display adapter has a hardware H.264 MFT",
        ));
    }
    let others = report
        .adapters
        .iter()
        .filter(|a| a.luid != display.luid && has_h264(a));
    if let Some(d) = others.clone().find(|a| is_discrete(a)) {
        return Some(make(
            HybridOption::A,
            &d.luid,
            "discrete adapter with a hardware H.264 MFT; DWM copies across",
        ));
    }
    if let Some(o) = others.clone().next() {
        return Some(make(
            HybridOption::A,
            &o.luid,
            "other adapter with a hardware H.264 MFT; DWM copies across",
        ));
    }
    Some(make(
        HybridOption::Software,
        "",
        "no hardware H.264 MFT: software MFT, capped at 1080p30",
    ))
}

/// Choices for every output in the report.
pub fn choose_all(report: &AdapterReport) -> Vec<Choice> {
    let mut monitors: Vec<u32> = report
        .adapters
        .iter()
        .flat_map(|a| a.outputs.iter().map(|o| o.monitor_index))
        .collect();
    monitors.sort_unstable();
    monitors
        .into_iter()
        .filter_map(|m| choose(report, m))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EncoderInfo, OutputInfo};

    fn adapter(luid: &str, mem: u64, software: bool, h264: bool, monitors: &[u32]) -> AdapterInfo {
        AdapterInfo {
            luid: luid.into(),
            dedicated_video_memory_mb: mem,
            software,
            outputs: monitors
                .iter()
                .map(|m| OutputInfo {
                    monitor_index: *m,
                    ..OutputInfo::default()
                })
                .collect(),
            encoders: if h264 {
                vec![EncoderInfo {
                    codec: "h264".into(),
                    hardware: true,
                    ..EncoderInfo::default()
                }]
            } else {
                vec![]
            },
            ..AdapterInfo::default()
        }
    }

    fn report(adapters: Vec<AdapterInfo>) -> AdapterReport {
        AdapterReport {
            adapters,
            ..AdapterReport::default()
        }
    }

    #[test]
    fn single_igpu_is_option_c() {
        let r = report(vec![adapter("igpu", 128, false, true, &[0])]);
        let c = choose(&r, 0).unwrap_or_else(|| panic!("no choice"));
        assert_eq!(c.option, HybridOption::C);
        assert_eq!(
            (c.capture_adapter.as_str(), c.encode_adapter.as_str()),
            ("igpu", "igpu")
        );
    }

    #[test]
    fn hybrid_display_on_igpu_without_mft_uses_dgpu() {
        let r = report(vec![
            adapter("igpu", 128, false, false, &[0]),
            adapter("dgpu", 8192, false, true, &[]),
        ]);
        let c = choose(&r, 0).unwrap_or_else(|| panic!("no choice"));
        assert_eq!(c.option, HybridOption::A);
        assert_eq!(c.encode_adapter, "dgpu");
        assert_eq!(c.capture_adapter, "dgpu");
    }

    #[test]
    fn hybrid_prefers_same_adapter_when_it_can_encode() {
        let r = report(vec![
            adapter("igpu", 128, false, true, &[0]),
            adapter("dgpu", 8192, false, true, &[1]),
        ]);
        assert_eq!(choose(&r, 0).map(|c| c.encode_adapter), Some("igpu".into()));
        assert_eq!(choose(&r, 1).map(|c| c.encode_adapter), Some("dgpu".into()));
        assert_eq!(choose_all(&r).len(), 2);
    }

    #[test]
    fn no_mft_falls_back_to_software() {
        let r = report(vec![adapter("vm", 0, false, false, &[0])]);
        assert_eq!(
            choose(&r, 0).map(|c| c.option),
            Some(HybridOption::Software)
        );
    }

    #[test]
    fn software_adapter_never_encodes() {
        let r = report(vec![adapter("basic", 0, true, true, &[0])]);
        assert_eq!(
            choose(&r, 0).map(|c| c.option),
            Some(HybridOption::Software)
        );
    }

    #[test]
    fn unknown_monitor() {
        let r = report(vec![adapter("igpu", 128, false, true, &[0])]);
        assert!(choose(&r, 3).is_none());
    }
}
