//! Command line of `streamcore-video-windows-bench`.

pub const USAGE: &str = "\
usage: streamcore-video-windows-bench COMMAND [options]

commands:
  report                adapters, outputs, encoder MFTs per adapter, WGC support, the adapter policy
  probe                 create the hardware H.264 MFT on the chosen adapter and measure
                        min_inputs_before_output and one-in-flight encode time
  bench                 capture -> convert -> encode through a StreamController for --seconds
  encoders              diagnostics: the registered video encoders, what each Media Foundation
                        enumeration returns (with and without the hardware, adapter and
                        sort-and-filter options), activation results, and MF registry settings

options:
  --mode synthetic|desktop   bench source: a synthetic window over the whole monitor that draws a
                             frame-counter barcode and flips colour (default), or the desktop as is
  --pace composition|MS      synthetic: a new counter on every DWM composition (default) or every MS ms
  --monitor N                output index from `report` (default 0)
  --adapter auto|N|LUID      encoder/capture adapter: the policy's choice (default), an index, or a LUID
  --fps N                    target frame rate (default 60)
  --seconds S                measured seconds (default 30)
  --warmup S                 seconds before measuring (default 2)
  --size native|WxH          encode size (default native, rounded down to even)
  --bitrate BPS              target bitrate (default 0.1 bit per pixel per frame)
  --permits N                frames in flight, 1-3 (default: from the probe)
  --buffers N                WGC frame-pool buffers (default permits + 3)
  --no-min-update-interval   do not set GraphicsCaptureSession.MinUpdateInterval
  --cursor                   capture the cursor
  --timer-period on|off      hold a 1 ms timer period while capturing (bench) or encoding (probe),
                             with the power-throttling opt-out (default on)
  --gap MS                   probe: then time frames fed MS ms apart (a sparse, idle-like source)
  --keep-warm off|on|MS      probe with --gap: re-encode the last frame every refresh (on) or every
                             MS ms between the spaced frames (default off)
  --keep-warm-hold MS        bench: the stream's keep-warm (PacingPolicy::keep_warm_ms, with rate
                             adaptation on) with this hold, input simulated every 50 ms through
                             the run (default off)
  --warm-session native|WxH  probe with --keep-warm: encode the fill frames on a second encoder
                             session of that size, its output discarded
  --rate-control MODE        encoder rate control: vbr (default, as streams use), cbr, pcvbr
                             (peak-constrained VBR, peak --peak x the mean), quality or lowdelayvbr
  --peak X                   pcvbr: the peak bitrate as a multiple of the mean (default 2)
  --content flat|text        synthetic: flat colour (default) or a detailed text-like page, which
                             with --pace MS scrolls a screen's worth of new detail every MS ms
  --json PATH                also write the report as JSON
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Report,
    Probe,
    Bench,
    Encoders,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Synthetic,
    Desktop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pace {
    Composition,
    EveryMs(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    Native,
    Fixed(u32, u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdapterSel {
    Auto,
    Index(u32),
    Luid(u64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub command: Command,
    pub mode: Mode,
    pub pace: Pace,
    pub monitor: u32,
    pub adapter: AdapterSel,
    pub fps: u32,
    pub seconds: f64,
    pub warmup: f64,
    pub size: Size,
    pub bitrate: Option<u64>,
    pub permits: Option<u8>,
    pub buffers: Option<u8>,
    pub min_update_interval: bool,
    pub cursor: bool,
    pub timer_period: bool,
    /// `--keep-warm`: None off, Some(0) every refresh, Some(ms).
    pub keep_warm: Option<u32>,
    /// `--gap`: the probe's spaced frames, MS apart.
    pub gap: Option<u32>,
    /// `--keep-warm-hold`: the bench stream's keep-warm hold.
    pub keep_warm_hold: Option<u32>,
    /// `--rate-control`: an `eAVEncCommonRateControlMode` value (2 unconstrained VBR).
    pub rate_control: u32,
    /// `--peak`: pcvbr's peak as a multiple of the mean.
    pub peak_ratio: f64,
    /// `--content text`: the synthetic window draws a detailed page.
    pub text_content: bool,
    /// `--warm-session`: the probe's second session size.
    pub warm_session: Option<Size>,
    pub json: Option<String>,
}

impl Options {
    fn new(command: Command) -> Self {
        Self {
            command,
            mode: Mode::Synthetic,
            pace: Pace::Composition,
            monitor: 0,
            adapter: AdapterSel::Auto,
            fps: 60,
            seconds: 30.0,
            warmup: 2.0,
            size: Size::Native,
            bitrate: None,
            permits: None,
            buffers: None,
            min_update_interval: true,
            cursor: false,
            timer_period: true,
            keep_warm: None,
            gap: None,
            keep_warm_hold: None,
            rate_control: 2,
            peak_ratio: 2.0,
            text_content: false,
            warm_session: None,
            json: None,
        }
    }

    /// The peak bitrate multiple the rate control takes (pcvbr only).
    pub fn peak(&self) -> Option<f64> {
        (self.rate_control == 1).then_some(self.peak_ratio)
    }

    /// The bitrate for `width` x `height`: the option, else 0.1 bit per pixel per frame.
    pub fn bitrate_for(&self, width: u32, height: u32) -> u64 {
        self.bitrate.unwrap_or_else(|| {
            (u64::from(width) * u64::from(height) * u64::from(self.fps) / 10).max(500_000)
        })
    }
}

fn num<T: std::str::FromStr>(flag: &str, v: Option<String>) -> Result<T, String> {
    let v = v.ok_or_else(|| format!("{flag} needs a value"))?;
    v.parse().map_err(|_| format!("{flag}: bad value {v}"))
}

pub fn parse(args: Vec<String>) -> Result<Options, String> {
    let mut it = args.into_iter();
    let command = match it.next().as_deref() {
        Some("report") => Command::Report,
        Some("probe") => Command::Probe,
        Some("bench") => Command::Bench,
        Some("encoders") => Command::Encoders,
        Some(other) => return Err(format!("unknown command {other}")),
        None => return Err("missing command".into()),
    };
    let mut o = Options::new(command);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--mode" => {
                o.mode = match it.next().as_deref() {
                    Some("synthetic") => Mode::Synthetic,
                    Some("desktop") => Mode::Desktop,
                    other => return Err(format!("--mode: bad value {other:?}")),
                }
            }
            "--pace" => {
                o.pace = match it.next() {
                    Some(v) if v == "composition" => Pace::Composition,
                    Some(v) => Pace::EveryMs(
                        v.parse()
                            .ok()
                            .filter(|ms| *ms > 0)
                            .ok_or(format!("--pace: bad value {v}"))?,
                    ),
                    None => return Err("--pace needs a value".into()),
                }
            }
            "--monitor" => o.monitor = num(&flag, it.next())?,
            "--adapter" => {
                let v = it.next().ok_or("--adapter needs a value")?;
                o.adapter = if v == "auto" {
                    AdapterSel::Auto
                } else if v.len() == 16 {
                    AdapterSel::Luid(
                        u64::from_str_radix(&v, 16).map_err(|_| format!("bad LUID {v}"))?,
                    )
                } else {
                    AdapterSel::Index(v.parse().map_err(|_| format!("bad adapter {v}"))?)
                };
            }
            "--fps" => {
                o.fps = num(&flag, it.next())?;
                if !(1..=240).contains(&o.fps) {
                    return Err("--fps must be 1-240".into());
                }
            }
            "--seconds" => o.seconds = num(&flag, it.next())?,
            "--warmup" => o.warmup = num(&flag, it.next())?,
            "--size" => {
                let v = it.next().ok_or("--size needs a value")?;
                o.size = if v == "native" {
                    Size::Native
                } else {
                    let (w, h) = v.split_once('x').ok_or(format!("bad size {v}"))?;
                    let w: u32 = w.parse().map_err(|_| format!("bad size {v}"))?;
                    let h: u32 = h.parse().map_err(|_| format!("bad size {v}"))?;
                    if w < 16 || h < 16 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
                        return Err(format!("size {v}: even, at least 16x16"));
                    }
                    Size::Fixed(w, h)
                };
            }
            "--bitrate" => o.bitrate = Some(num(&flag, it.next())?),
            "--permits" => {
                let p: u8 = num(&flag, it.next())?;
                if !(1..=3).contains(&p) {
                    return Err("--permits must be 1-3".into());
                }
                o.permits = Some(p);
            }
            "--buffers" => {
                let b: u8 = num(&flag, it.next())?;
                if !(1..=16).contains(&b) {
                    return Err("--buffers must be 1-16".into());
                }
                o.buffers = Some(b);
            }
            "--no-min-update-interval" => o.min_update_interval = false,
            "--cursor" => o.cursor = true,
            "--timer-period" => {
                o.timer_period = match it.next().as_deref() {
                    Some("on") => true,
                    Some("off") => false,
                    other => return Err(format!("--timer-period: bad value {other:?}")),
                }
            }
            "--keep-warm" => {
                o.keep_warm = match it.next().as_deref() {
                    Some("off") => None,
                    Some("on") => Some(0),
                    Some(v) => Some(
                        v.parse()
                            .ok()
                            .filter(|&ms| ms > 0)
                            .ok_or(format!("--keep-warm: bad value {v}"))?,
                    ),
                    None => return Err("--keep-warm needs a value".into()),
                }
            }
            "--keep-warm-hold" => {
                let ms: u32 = num("--keep-warm-hold", it.next())?;
                if ms == 0 {
                    return Err("--keep-warm-hold must be > 0".into());
                }
                o.keep_warm_hold = Some(ms);
            }
            "--gap" => {
                let ms: u32 = num("--gap", it.next())?;
                if ms == 0 {
                    return Err("--gap must be > 0".into());
                }
                o.gap = Some(ms);
            }
            "--warm-session" => {
                o.warm_session = Some(match it.next().as_deref() {
                    Some("native") => Size::Native,
                    Some(v) => {
                        let (w, h) = v
                            .split_once('x')
                            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                            .filter(|&(w, h): &(u32, u32)| w >= 2 && h >= 2)
                            .ok_or(format!("--warm-session: bad value {v}"))?;
                        Size::Fixed(w & !1, h & !1)
                    }
                    None => return Err("--warm-session needs a value".into()),
                })
            }
            "--rate-control" => {
                o.rate_control = match it.next().as_deref() {
                    Some("cbr") => 0,
                    Some("pcvbr") => 1,
                    Some("vbr") => 2,
                    Some("quality") => 3,
                    Some("lowdelayvbr") => 4,
                    other => return Err(format!("--rate-control: bad value {other:?}")),
                }
            }
            "--peak" => {
                let k: f64 = num("--peak", it.next())?;
                if k.is_nan() || k < 1.0 {
                    return Err("--peak must be >= 1".into());
                }
                o.peak_ratio = k;
            }
            "--content" => {
                o.text_content = match it.next().as_deref() {
                    Some("flat") => false,
                    Some("text") => true,
                    other => return Err(format!("--content: bad value {other:?}")),
                }
            }
            "--json" => o.json = Some(it.next().ok_or("--json needs a path")?),
            other => return Err(format!("unknown option {other}")),
        }
    }
    if !(o.seconds > 0.0 && o.warmup >= 0.0) {
        return Err("--seconds must be > 0 and --warmup >= 0".into());
    }
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn defaults() {
        let o = parse(args("bench")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(o.command, Command::Bench);
        assert_eq!(
            (o.mode, o.pace, o.fps),
            (Mode::Synthetic, Pace::Composition, 60)
        );
        assert_eq!(o.size, Size::Native);
        assert!(o.min_update_interval);
        assert_eq!(o.bitrate_for(1024, 768), 4_718_592);
    }

    #[test]
    fn options() {
        let o = parse(args(
            "bench --mode desktop --pace 50 --monitor 1 --adapter 00000000000a1b2c --fps 30 \
             --seconds 5 --warmup 0 --size 1280x720 --bitrate 6000000 --permits 2 --buffers 4 \
             --no-min-update-interval --cursor --json out.json",
        ))
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(o.mode, Mode::Desktop);
        assert_eq!(o.pace, Pace::EveryMs(50));
        assert_eq!(o.adapter, AdapterSel::Luid(0xa1b2c));
        assert_eq!(o.size, Size::Fixed(1280, 720));
        assert_eq!((o.permits, o.buffers), (Some(2), Some(4)));
        assert_eq!(o.bitrate_for(1, 1), 6_000_000);
        assert!(!o.min_update_interval && o.cursor);
        assert_eq!(o.json.as_deref(), Some("out.json"));
        assert_eq!(
            parse(args("probe --adapter 1"))
                .unwrap_or_else(|e| panic!("{e}"))
                .adapter,
            AdapterSel::Index(1)
        );
    }

    #[test]
    fn errors() {
        for bad in [
            "",
            "fly",
            "bench --fps 0",
            "bench --permits 4",
            "bench --size 1281x720",
            "bench --pace 0",
            "bench --mode nope",
            "bench --seconds",
            "bench --bogus",
        ] {
            assert!(parse(args(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn keep_warm() {
        assert_eq!(parse(args("bench")).unwrap().keep_warm, None);
        assert_eq!(
            parse(args("bench --keep-warm on")).unwrap().keep_warm,
            Some(0)
        );
        assert_eq!(
            parse(args("bench --keep-warm 8")).unwrap().keep_warm,
            Some(8)
        );
        assert_eq!(
            parse(args("bench --keep-warm off")).unwrap().keep_warm,
            None
        );
        assert!(parse(args("bench --keep-warm 0")).is_err());
        assert!(parse(args("bench --keep-warm")).is_err());
        assert_eq!(parse(args("probe --gap 500")).unwrap().gap, Some(500));
        assert_eq!(
            parse(args("bench --keep-warm-hold 1000"))
                .unwrap()
                .keep_warm_hold,
            Some(1000)
        );
        assert_eq!(parse(args("bench")).unwrap().rate_control, 2);
        assert!(parse(args("probe --gap 0")).is_err());
        assert_eq!(
            parse(args("probe --warm-session 1920x1080"))
                .unwrap()
                .warm_session,
            Some(Size::Fixed(1920, 1080))
        );
        assert!(parse(args("probe --warm-session 0x0")).is_err());
        assert_eq!(
            parse(args("probe --rate-control quality"))
                .unwrap()
                .rate_control,
            3
        );
        assert!(parse(args("probe --rate-control fast")).is_err());
        let o = parse(args("bench --rate-control pcvbr --peak 3 --content text")).unwrap();
        assert_eq!((o.peak(), o.text_content), (Some(3.0), true));
        assert_eq!(
            parse(args("bench --rate-control vbr")).unwrap().peak(),
            None
        );
        assert!(parse(args("bench --peak 0.5")).is_err());
    }
}
