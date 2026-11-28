//! The Windows backends (the `backends` feature) and, with the `bench` feature, the bench
//! commands. Only `sys` uses `unsafe`.

#[cfg(feature = "bench")]
pub mod bench;
pub mod capture;
pub mod convert;
#[cfg(feature = "bench")]
pub mod diag;
pub mod encoder;
pub mod probe;
pub mod queue;
#[cfg(feature = "bench")]
pub mod report;
pub mod surfaces;
pub mod sys;

#[cfg(feature = "bench")]
use crate::cli::{AdapterSel, Command, Options, Size};
#[cfg(feature = "bench")]
use crate::model::AdapterReport;
#[cfg(feature = "bench")]
use serde::Serialize;
#[cfg(feature = "bench")]
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
#[cfg(feature = "bench")]
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
#[cfg(feature = "bench")]
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

#[cfg(feature = "bench")]
fn write_json<T: Serialize>(path: Option<&str>, value: &T) -> Result<(), Error> {
    let json = serde_json::to_string_pretty(value).map_err(err("serialise"))?;
    match path {
        Some(p) => std::fs::write(p, json + "\n").map_err(err("write json")),
        None => Ok(()),
    }
}

/// Runs one command.
#[cfg(feature = "bench")]
pub fn run(options: &Options) -> Result<(), Error> {
    // The process MTA: helper threads (encoder create, bounded invalidate) join it.
    let _apartment = sys::com::Apartment::mta().map_err(err("CoInitializeEx"))?;
    let _mf = sys::com::MediaFoundation::start().map_err(err("MFStartup"))?;
    sys::mf::force_direct(options.direct_mft);
    let adapters = report::build();
    // Runs measure a lit screen: the display going to sleep mid-run changes what they measure.
    let _awake = sys::com::DisplayAwake::acquire();
    match options.command {
        Command::Report => {
            report::print(&adapters);
            write_json(options.json.as_deref(), &adapters)
        }
        Command::Probe => {
            let (luid, output) = select(options, &adapters)?;
            let (_, encode) = sizes(&output, options.size);
            let bitrate = options.bitrate_for(encode.0, encode.1);
            // The probe has no capture: it holds the period itself, as a stream would.
            let _timer = options.timer_period.then(sys::com::TimerPeriod::acquire);
            let gap = options.gap.map(|ms| probe::Gap {
                every: std::time::Duration::from_millis(u64::from(ms)),
                keep_warm: options.keep_warm.map(|ms| match ms {
                    0 => std::time::Duration::from_nanos(
                        capture::refresh_interval_ns(&output).unwrap_or(16_666_667),
                    ),
                    ms => std::time::Duration::from_millis(u64::from(ms)),
                }),
                warm_session: options.warm_session.map(|size| match size {
                    Size::Native => encode,
                    Size::Fixed(w, h) => (w, h),
                }),
            });
            let mut p = probe::run(
                luid,
                encode.0,
                encode.1,
                options.fps,
                bitrate,
                options.rate_control,
                options.peak(),
                gap,
            )?;
            p.timer = timer_report(options.timer_period);
            probe::print(&p);
            write_json(options.json.as_deref(), &p)
        }
        Command::Encoders => {
            let d = diag::run();
            diag::print(&d);
            write_json(options.json.as_deref(), &d)
        }
        Command::Bench => {
            let window = match &options.window {
                Some(title) => Some(bench::find_window(title)?),
                None => None,
            };
            // A window is measured on the display most of it is on.
            let mut options = options.clone();
            if let Some(monitor) = window.as_ref().and_then(bench::window_monitor) {
                options.monitor = monitor;
            }
            let options = &options;
            let (luid, output) = select(options, &adapters)?;
            let b = bench::run(options, luid, &output, window.as_ref())?;
            bench::print(&b);
            write_json(options.json.as_deref(), &b)
        }
    }
}

/// The timer period as requested and as the first hold applied it.
#[cfg(feature = "bench")]
pub(crate) fn timer_report(requested: bool) -> crate::model::TimerReport {
    let status = sys::com::TimerPeriod::status();
    crate::model::TimerReport {
        requested,
        period_1ms: requested && status.period_1ms,
        throttling_opt_out: requested && status.throttling_opt_out,
        throttling_unreadable: requested && status.throttling_unreadable,
    }
}
