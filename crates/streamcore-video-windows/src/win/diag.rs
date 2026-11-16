//! The `encoders` command: why a PC's hardware video encoders are, or are not, offered to
//! StreamCore. Media Foundation hides a registered hardware encoder from enumeration for several
//! reasons (its adapter, the sort-and-filter policy, a failed load); this runs the same search
//! StreamCore and ffmpeg's `h264_mf` use next to looser ones, and lists what is registered.

use super::os_build;
use super::sys::d3d::enumerate_adapters;
use super::sys::mfdiag::{self, enumerate};
use crate::model::{EncoderDiag, EnumCall};
use windows::Win32::Media::MediaFoundation::{
    MFT_ENUM_FLAG, MFT_ENUM_FLAG_ALL, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
};

pub fn run() -> EncoderDiag {
    let mut d = EncoderDiag {
        os_build: os_build(),
        ..EncoderDiag::default()
    };
    let adapters = enumerate_adapters().unwrap_or_default();
    d.adapters = adapters
        .iter()
        .map(|a| {
            (
                a.index,
                a.name.clone(),
                format!("{:016x}", a.luid),
                format!("{:04x}", a.vendor_id),
            )
        })
        .collect();

    let (nv12, h264) = (Some(mfdiag::nv12()), Some(mfdiag::h264()));
    let hw_sorted = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
    let hw = MFT_ENUM_FLAG_HARDWARE;
    let all_sorted = MFT_ENUM_FLAG_ALL | MFT_ENUM_FLAG_SORTANDFILTER;
    let all = MFT_ENUM_FLAG_ALL;
    let mut calls: Vec<EnumCall> = Vec::new();
    let flag_sets: [(&str, MFT_ENUM_FLAG); 4] = [
        ("hardware, sorted (as StreamCore and h264_mf)", hw_sorted),
        ("hardware, unfiltered", hw),
        ("all, sorted", all_sorted),
        ("all, unfiltered", all),
    ];
    let hardware = |f: MFT_ENUM_FLAG| f.0 & MFT_ENUM_FLAG_HARDWARE.0 != 0;
    // NV12 -> H.264, as StreamCore asks; any -> H.264; any encoder at all.
    for (types, input, output) in [
        ("NV12 -> H264", nv12, h264),
        ("any -> H264", None, h264),
        ("any -> any", None, None),
    ] {
        for (what, flags) in flag_sets {
            calls.push(enumerate(
                &format!("{types}, {what}"),
                false,
                flags,
                input,
                output,
                None,
                hardware(flags),
            ));
        }
    }
    // MFTEnum2 without an adapter, then scoped to each adapter (StreamCore's own call).
    for (what, flags) in [flag_sets[0], flag_sets[1]] {
        calls.push(enumerate(
            &format!("NV12 -> H264, {what}, no adapter"),
            true,
            flags,
            nv12,
            h264,
            None,
            true,
        ));
        for a in adapters.iter().filter(|a| !a.software) {
            calls.push(enumerate(
                &format!("NV12 -> H264, {what}, adapter {} {}", a.index, a.name),
                true,
                flags,
                nv12,
                h264,
                Some(a.luid),
                true,
            ));
        }
    }
    d.calls = calls;
    d.registered = mfdiag::registered();
    d.mf_settings = mfdiag::mf_settings();
    d
}

pub fn print(d: &EncoderDiag) {
    println!("OS build {}", d.os_build);
    for (index, name, luid, vendor) in &d.adapters {
        println!("adapter {index} {name}  vendor {vendor}  luid {luid}");
    }
    println!();
    println!("Enumeration (count: names)");
    for c in &d.calls {
        let found: Vec<String> = c
            .found
            .iter()
            .map(|e| match &e.activation {
                Some(a) if a != "ok" => format!("{} [activate: {a}]", e.name),
                _ => e.name.clone(),
            })
            .collect();
        match &c.error {
            Some(e) => println!("  {} {:<70} error {e}", c.api, c.label),
            None => println!(
                "  {} {:<70} {}: {}",
                c.api,
                c.label,
                c.found.len(),
                found.join("; ")
            ),
        }
    }
    println!();
    println!("Every encoder returned (first time seen)");
    let mut seen = std::collections::BTreeSet::new();
    for e in d.calls.iter().flat_map(|c| c.found.iter()) {
        let key = e.clsid.clone().unwrap_or_else(|| e.name.clone());
        if seen.insert(key.clone()) {
            println!(
                "  {}  clsid {}  flags {:?}  merit {:?}  vendor {:?}  url {:?}  adapter {:?}  activation {:?}",
                e.name,
                key,
                e.flags,
                e.merit,
                e.vendor_id,
                e.hardware_url,
                e.adapter_luid,
                e.activation
            );
        }
    }
    println!();
    println!("Registered video encoders (encoder category)");
    for m in &d.registered {
        println!(
            "  {} {}  MFTFlags {:?}  dll {:?}  threading {:?}  create {:?}  url {:?}  d3d11 {:?}",
            m.clsid,
            m.name,
            m.mft_flags,
            m.dll,
            m.threading,
            m.create,
            m.hardware_url,
            m.d3d11_aware
        );
    }
    println!();
    println!(
        "Media Foundation registry settings (HKLM\\SOFTWARE\\Microsoft\\Windows Media Foundation)"
    );
    for v in &d.mf_settings {
        println!("  {} = {}", v.name, v.value);
    }
}
