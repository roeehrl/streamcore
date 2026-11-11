//! The adapter report: adapters, the outputs each drives, which has a hardware H.264 MFT
//! (`MFTEnum2` per LUID), whether its video processor converts BGRA to NV12, WGC support, and the
//! adapter policy's choice per output.

use super::os_build;
use super::sys::d3d::{Gpu, enumerate_adapters};
use super::sys::mf::enumerate_h264;
use super::sys::vp::VideoProcessor;
use crate::model::{AdapterInfo, AdapterReport, OutputInfo};
use windows::Foundation::Metadata::ApiInformation;
use windows::Graphics::Capture::GraphicsCaptureSession;
use windows::core::h;

fn colour_space_name(cs: i32) -> String {
    match cs {
        0 => "RGB_FULL_G22_NONE_P709 (sRGB)".into(),
        1 => "RGB_FULL_G10_NONE_P709 (scRGB)".into(),
        12 => "RGB_FULL_G2084_NONE_P2020 (HDR10)".into(),
        -1 => "unknown".into(),
        other => format!("DXGI_COLOR_SPACE_TYPE {other}"),
    }
}

pub fn build() -> AdapterReport {
    let mut report = AdapterReport {
        os_build: os_build(),
        wgc_supported: GraphicsCaptureSession::IsSupported().unwrap_or(false),
        min_update_interval_present: ApiInformation::IsPropertyPresent(
            h!("Windows.Graphics.Capture.GraphicsCaptureSession"),
            h!("MinUpdateInterval"),
        )
        .unwrap_or(false),
        ..AdapterReport::default()
    };
    let adapters = match enumerate_adapters() {
        Ok(a) => a,
        Err(e) => {
            report.adapters.push(AdapterInfo {
                name: format!("enumeration failed: {e}"),
                ..AdapterInfo::default()
            });
            return report;
        }
    };
    let mut monitor_index = 0;
    for a in &adapters {
        let mut info = AdapterInfo {
            index: a.index,
            name: a.name.clone(),
            vendor_id: a.vendor_id,
            device_id: a.device_id,
            luid: format!("{:016x}", a.luid),
            dedicated_video_memory_mb: a.dedicated_video_memory >> 20,
            shared_system_memory_mb: a.shared_system_memory >> 20,
            software: a.software,
            ..AdapterInfo::default()
        };
        if !a.software {
            info.bgra_to_nv12 = Gpu::create(Some(a.luid))
                .ok()
                .and_then(|g| VideoProcessor::supports_bgra_to_nv12(&g).ok());
        }
        for o in &a.outputs {
            info.outputs.push(OutputInfo {
                monitor_index,
                device_name: o.device_name.clone(),
                desktop: o.desktop,
                attached: o.attached,
                bits_per_color: o.bits_per_color,
                color_space: colour_space_name(o.color_space),
                refresh_hz: o.refresh_hz,
            });
            monitor_index += 1;
        }
        match enumerate_h264(true, Some(a.luid)) {
            Ok(list) => info.encoders.extend(list.into_iter().map(|c| c.info)),
            Err(e) => info.encoder_enum_error = Some(e.to_string()),
        }
        report.adapters.push(info);
    }
    report.software_encoders = enumerate_h264(false, None)
        .map(|l| l.into_iter().map(|c| c.info).collect())
        .unwrap_or_default();
    report.policy = crate::policy::choose_all(&report);
    report
}

pub fn print(r: &AdapterReport) {
    println!("OS build {}", r.os_build);
    println!(
        "WGC supported: {}   MinUpdateInterval present: {}",
        r.wgc_supported, r.min_update_interval_present
    );
    for a in &r.adapters {
        println!(
            "\nadapter {} {}  vendor {:04x} device {:04x}  luid {}  dedicated {} MiB  shared {} MiB{}",
            a.index,
            a.name,
            a.vendor_id,
            a.device_id,
            a.luid,
            a.dedicated_video_memory_mb,
            a.shared_system_memory_mb,
            if a.software { "  SOFTWARE" } else { "" }
        );
        if let Some(vp) = a.bgra_to_nv12 {
            println!("  video processor BGRA -> NV12: {vp}");
        }
        for o in &a.outputs {
            println!(
                "  monitor {}  {}  desktop {:?}  attached {}  {} bpc  {}  refresh {}",
                o.monitor_index,
                o.device_name,
                o.desktop,
                o.attached,
                o.bits_per_color,
                o.color_space,
                o.refresh_hz.map_or("?".into(), |h| format!("{h} Hz")),
            );
        }
        if a.outputs.is_empty() {
            println!("  (drives no output)");
        }
        for e in &a.encoders {
            println!(
                "  hardware encoder {} {}  vendor {}",
                e.codec,
                e.name,
                e.vendor_id.as_deref().unwrap_or("-")
            );
        }
        if a.encoders.is_empty() {
            println!("  no hardware H.264 encoder MFT");
        }
        if let Some(e) = &a.encoder_enum_error {
            println!("  encoder enumeration error: {e}");
        }
    }
    println!();
    for e in &r.software_encoders {
        println!("software encoder {} {}", e.codec, e.name);
    }
    for c in &r.policy {
        println!(
            "policy: monitor {} -> option {:?}, capture on {}, encode on {} ({})",
            c.monitor_index,
            c.option,
            c.capture_adapter,
            if c.encode_adapter.is_empty() {
                "software"
            } else {
                &c.encode_adapter
            },
            c.reason
        );
    }
}
