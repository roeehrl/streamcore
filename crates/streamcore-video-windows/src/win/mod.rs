//! The Windows backends and the bench commands. Only `sys` uses `unsafe`.

pub mod bench;
pub mod capture;
pub mod convert;
pub mod encoder;
pub mod probe;
pub mod queue;
pub mod report;
pub mod surfaces;
pub mod sys;

use crate::cli::{AdapterSel, Command, Options, Size};
use crate::model::AdapterReport;
use serde::Serialize;
use sys::d3d::{OutputDesc, enumerate_adapters};

pub type Error = String;

fn err<E: std::fmt::Display>(context: &str) -> impl FnOnce(E) -> Error + '_ {
    move |e| format!("{context}: {e}")
}

/// The OS version as `major.minor.build.revision`, from WinRT.
pub fn os_build() -> String {
    windows::System::Profile::AnalyticsInfo::VersionInfo()
        .and_then(|v| v.DeviceFamilyVersion())
        .ok()
        .and_then(|s| s.to_string().parse::<u64>().ok())
        .map(|v| {
            format!(
                "{}.{}.{}.{}",
                v >> 48,
                (v >> 32) & 0xffff,
                (v >> 16) & 0xffff,
                v & 0xffff
            )
        })
        .unwrap_or_else(|| "unknown".into())
}

/// The adapter (LUID) a run uses and the monitor's output: `--adapter`, or the policy's choice.
pub fn select(options: &Options, report: &AdapterReport) -> Result<(u64, OutputDesc), Error> {
    let adapters = enumerate_adapters().map_err(err("enumerate adapters"))?;
    let output = adapters
        .iter()
        .flat_map(|a| a.outputs.iter())
        .nth(options.monitor as usize)
        .cloned()
        .ok_or_else(|| format!("no monitor {}", options.monitor))?;
    let luid = match &options.adapter {
        AdapterSel::Index(i) => {
            adapters
                .get(*i as usize)
                .ok_or_else(|| format!("no adapter {i}"))?
                .luid
        }
        AdapterSel::Luid(l) => *l,
        AdapterSel::Auto => {
            let choice = crate::policy::choose(report, options.monitor)
                .ok_or_else(|| format!("policy has no choice for monitor {}", options.monitor))?;
            u64::from_str_radix(&choice.encode_adapter, 16)
                .map_err(|_| format!("policy chose no hardware encoder: {}", choice.reason))?
        }
    };
    Ok((luid, output))
}

/// The capture size of an output and the encode size for `size`.
pub fn sizes(output: &OutputDesc, size: Size) -> ((u32, u32), (u32, u32)) {
    let capture = (
        (output.desktop[2] - output.desktop[0]).max(2) as u32,
        (output.desktop[3] - output.desktop[1]).max(2) as u32,
    );
    let encode = match size {
        Size::Native => (capture.0 & !1, capture.1 & !1),
        Size::Fixed(w, h) => (w, h),
    };
    (capture, encode)
}

fn write_json<T: Serialize>(path: Option<&str>, value: &T) -> Result<(), Error> {
    let json = serde_json::to_string_pretty(value).map_err(err("serialise"))?;
    match path {
        Some(p) => std::fs::write(p, json + "\n").map_err(err("write json")),
        None => Ok(()),
    }
}

/// Runs one command.
pub fn run(options: &Options) -> Result<(), Error> {
    // The process MTA: helper threads (encoder create, bounded invalidate) join it.
    let _apartment = sys::com::Apartment::mta().map_err(err("CoInitializeEx"))?;
    let _mf = sys::com::MediaFoundation::start().map_err(err("MFStartup"))?;
    let _timer = options.timer_1ms.then(sys::com::TimerPeriod::one_ms);
    let adapters = report::build();
    match options.command {
        Command::Report => {
            report::print(&adapters);
            write_json(options.json.as_deref(), &adapters)
        }
        Command::Probe => {
            let (luid, output) = select(options, &adapters)?;
            let (_, encode) = sizes(&output, options.size);
            let bitrate = options.bitrate_for(encode.0, encode.1);
            let p = probe::run(luid, encode.0, encode.1, options.fps, bitrate)?;
            probe::print(&p);
            write_json(options.json.as_deref(), &p)
        }
        Command::Bench => {
            let (luid, output) = select(options, &adapters)?;
            let b = bench::run(options, luid, &output)?;
            bench::print(&b);
            write_json(options.json.as_deref(), &b)
        }
    }
}
