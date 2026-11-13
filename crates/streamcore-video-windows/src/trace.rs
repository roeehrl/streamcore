//! Per-frame stage stamps (design doc 9.2) and the recorder the Windows backends write them to.
//!
//! Every stamp is host-monotonic nanoseconds on the QPC clock: `SystemRelativeTime` (H0) is QPC
//! based, and every other stamp is `QueryPerformanceCounter` at the point named below. A frame is
//! followed through its ids: the capture surface, the converted surface and its fence value, then
//! the stream's `submit_seq`, which is also the wire sequence of its payload packet.
//!
//! | Stamp | Point |
//! | --- | --- |
//! | H0 | the composition's display time: `Direct3D11CaptureFrame.SystemRelativeTime`. On a native display this is when the frame reaches the screen, and it can fall after H1: DWM hands WGC the frame before the refresh that shows it (about 12 ms later under constant redraw at 60 Hz, at H1 on an idle desktop) |
//! | H1 | `FrameArrived` handler entry, on the executor thread |
//! | H2c | convert recorded: `VideoProcessorBlt` and the fence `Signal` returned (CPU) |
//! | H2 | convert GPU-complete: the fence reached the frame's value (a waiting thread stamps it) |
//! | H3 | encoder input accepted: `ProcessInput` returned |
//! | H4 | access unit available: `ProcessOutput` returned on the MFT event thread |
//! | H5 | `VideoSink` push on the executor (the bench's discard sink does not stamp it) |
//! | src | synthetic mode: the source finished drawing the counter this frame shows first |
//!
//! Recorder calls are leaf calls: one short lock, no calls out while it is held.

use crate::stats::{Summary, summarize_ns};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard};

/// Stamps of one submitted frame. `None`: not reached or not measured.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FrameTrace {
    pub seq: u64,
    pub h0: Option<u64>,
    pub h1: Option<u64>,
    pub h2c: Option<u64>,
    pub h2: Option<u64>,
    pub h3: Option<u64>,
    pub h4: Option<u64>,
    pub h5: Option<u64>,
    pub src: Option<u64>,
    /// Synthetic mode: the counter read back from the converted frame.
    pub counter: Option<u32>,
    pub fence: Option<u64>,
    pub bytes: Option<usize>,
    pub keyframe: bool,
}

/// `to - from` when `to` is not before `from`; `None` otherwise.
fn ordered(from: Option<u64>, to: Option<u64>) -> Option<u64> {
    match (from, to) {
        (Some(a), Some(b)) => b.checked_sub(a),
        _ => None,
    }
}

fn delta(from: Option<u64>, to: Option<u64>) -> Option<u64> {
    match (from, to) {
        (Some(a), Some(b)) => Some(b.saturating_sub(a)),
        _ => None,
    }
}

/// The stage rows a report prints, in order.
pub const STAGES: &[(&str, &str)] = &[
    ("H0->H1", "capture: present -> FrameArrived"),
    ("H1->H0", "FrameArrived -> on screen (H0 after H1)"),
    ("H1->H2c", "admission + convert recorded (CPU)"),
    ("H1->H2", "FrameArrived -> convert GPU-complete"),
    ("H2c->H3", "submit: permit/NeedInput wait + ProcessInput"),
    ("H3->H4", "encode: input accepted -> access unit"),
    ("H4->H5", "hop to the executor + sink push"),
    ("H1->H4", "FrameArrived -> encoded"),
    ("H0->H4", "present -> encoded"),
    ("H0->H5", "present -> sent"),
    ("src->H0", "synthetic draw -> present"),
    ("src->H1", "synthetic draw -> FrameArrived"),
    ("src->H4", "synthetic draw -> encoded"),
    ("src->H5", "synthetic draw -> sent"),
];

impl FrameTrace {
    /// The named stage's duration, if both stamps exist. Reversed stamps give 0.
    pub fn stage(&self, name: &str) -> Option<u64> {
        match name {
            // H0 and H1 come in either order on Windows: each row counts only its own order.
            "H0->H1" => ordered(self.h0, self.h1),
            "H1->H0" => ordered(self.h1, self.h0),
            "H1->H2c" => delta(self.h1, self.h2c),
            "H1->H2" => delta(self.h1, self.h2),
            "H2c->H3" => delta(self.h2c, self.h3),
            "H3->H4" => delta(self.h3, self.h4),
            "H4->H5" => delta(self.h4, self.h5),
            "H1->H4" => delta(self.h1, self.h4),
            "H0->H4" => delta(self.h0, self.h4),
            "H0->H5" => delta(self.h0, self.h5),
            "src->H0" => delta(self.src, self.h0),
            "src->H1" => delta(self.src, self.h1),
            "src->H4" => delta(self.src, self.h4),
            "src->H5" => delta(self.src, self.h5),
            _ => None,
        }
    }
}

/// Summaries of every stage over `frames`, keyed by stage name.
pub fn summarize_stages<'a>(
    frames: impl Iterator<Item = &'a FrameTrace> + Clone,
) -> BTreeMap<String, Summary> {
    let mut out = BTreeMap::new();
    for (name, _) in STAGES {
        let values: Vec<u64> = frames.clone().filter_map(|f| f.stage(name)).collect();
        if !values.is_empty() {
            out.insert((*name).to_string(), summarize_ns(&values));
        }
    }
    out
}

/// Counters the recorder keeps alongside the stamps.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    /// `FrameArrived` frames handed to the stream.
    pub captured: u64,
    /// Older frames the capture found queued behind the newest one and released unseen.
    pub pool_backlog: u64,
    pub converted: u64,
    pub submitted: u64,
    pub encoded: u64,
    pub sent: u64,
    /// The encoder consumed a frame without output, or failed it.
    pub encoder_dropped: u64,
}

#[derive(Default)]
struct Inner {
    captures: HashMap<u64, (Option<u64>, u64)>,
    capture_times: Vec<u64>,
    converts: HashMap<u64, FrameTrace>,
    fences: HashMap<u64, u64>,
    counters: HashMap<u64, u32>,
    frames: BTreeMap<u64, FrameTrace>,
    counts: Counts,
}

/// Collects stamps from the capture, converter, encoder and send hop.
#[derive(Default)]
pub struct Recorder {
    inner: Mutex<Inner>,
}

impl Recorder {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A capture surface was handed to the stream.
    pub fn captured(&self, surface: u64, h0: Option<u64>, h1: u64, backlog: u64) {
        let mut i = self.lock();
        i.captures.insert(surface, (h0, h1));
        i.capture_times.push(h1);
        i.counts.captured += 1;
        i.counts.pool_backlog += backlog;
    }

    /// The capture released `surface` (its stamps are no longer needed).
    pub fn capture_released(&self, surface: u64) {
        self.lock().captures.remove(&surface);
    }

    /// `input` was converted into `output`, signalled at `fence`.
    pub fn converted(&self, input: u64, output: u64, h2c: u64, fence: u64) {
        let mut i = self.lock();
        let (h0, h1) = i
            .captures
            .get(&input)
            .map_or((None, None), |(h0, h1)| (*h0, Some(*h1)));
        i.converts.insert(
            output,
            FrameTrace {
                h0,
                h1,
                h2c: Some(h2c),
                fence: Some(fence),
                ..FrameTrace::default()
            },
        );
        i.counts.converted += 1;
    }

    /// The converter released `output` without it being submitted.
    pub fn convert_released(&self, output: u64) {
        self.lock().converts.remove(&output);
    }

    /// The fence reached `value` at `at`.
    pub fn fence_done(&self, value: u64, at: u64) {
        self.lock().fences.insert(value, at);
    }

    /// The barcode read back from the frame signalled at `fence`.
    pub fn counter(&self, fence: u64, counter: u32) {
        self.lock().counters.insert(fence, counter);
    }

    /// `surface` was accepted by the encoder (`ProcessInput` returned at `h3`) as `seq`.
    pub fn submitted(&self, surface: u64, seq: u64, h3: u64) {
        let mut i = self.lock();
        let mut t = i.converts.remove(&surface).unwrap_or_default();
        t.seq = seq;
        t.h3 = Some(h3);
        i.frames.insert(seq, t);
        i.counts.submitted += 1;
    }

    /// The access unit of `seq` came out at `h4`.
    pub fn encoded(&self, seq: u64, h4: u64, bytes: usize, keyframe: bool) {
        let mut i = self.lock();
        if let Some(t) = i.frames.get_mut(&seq) {
            t.h4 = Some(h4);
            t.bytes = Some(bytes);
            t.keyframe = keyframe;
        }
        i.counts.encoded += 1;
    }

    pub fn encoder_dropped(&self) {
        self.lock().counts.encoder_dropped += 1;
    }

    /// The payload packet of `seq` was pushed to the `VideoSink` at `h5`.
    pub fn sent(&self, seq: u64, h5: u64) {
        let mut i = self.lock();
        if let Some(t) = i.frames.get_mut(&seq) {
            t.h5 = Some(h5);
        }
        i.counts.sent += 1;
    }

    /// Frames captured with H1 in `[from, to]`.
    pub fn captured_between(&self, from: u64, to: u64) -> u64 {
        self.lock()
            .capture_times
            .iter()
            .filter(|t| (from..=to).contains(*t))
            .count() as u64
    }

    pub fn counts(&self) -> Counts {
        self.lock().counts.clone()
    }

    /// Every submitted frame with H2 and the counter resolved, in sequence order.
    pub fn frames(&self) -> Vec<FrameTrace> {
        let i = self.lock();
        i.frames
            .values()
            .map(|t| {
                let mut t = t.clone();
                if let Some(f) = t.fence {
                    t.h2 = i.fences.get(&f).copied();
                    t.counter = i.counters.get(&f).copied();
                }
                t
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_followed_through_its_ids() {
        let r = Recorder::default();
        r.captured(11, Some(1_000), 2_000, 2);
        r.converted(11, 21, 2_500, 7);
        r.fence_done(7, 3_000);
        r.counter(7, 42);
        r.submitted(21, 1, 3_200);
        r.encoded(1, 9_000, 1200, true);
        r.sent(1, 9_100);
        let frames = r.frames();
        assert_eq!(frames.len(), 1);
        let t = &frames[0];
        assert_eq!(
            (t.h0, t.h1, t.h2c, t.h2, t.h3, t.h4, t.h5),
            (
                Some(1_000),
                Some(2_000),
                Some(2_500),
                Some(3_000),
                Some(3_200),
                Some(9_000),
                Some(9_100)
            )
        );
        assert_eq!(t.counter, Some(42));
        assert_eq!(t.stage("H0->H1"), Some(1_000));
        assert_eq!(t.stage("H2c->H3"), Some(700));
        assert_eq!(t.stage("H0->H5"), Some(8_100));
        let c = r.counts();
        assert_eq!((c.captured, c.pool_backlog, c.sent), (1, 2, 1));
        assert_eq!(r.captured_between(2_000, 2_000), 1);
        assert_eq!(r.captured_between(2_001, 9_000), 0);
    }

    #[test]
    fn a_reconvert_keeps_the_capture_stamps_and_releases_forget() {
        let r = Recorder::default();
        r.captured(11, None, 2_000, 0);
        r.converted(11, 21, 2_100, 1);
        r.convert_released(21);
        r.converted(11, 22, 2_200, 2);
        r.capture_released(11);
        r.submitted(22, 5, 2_300);
        r.submitted(99, 6, 2_400); // unknown surface: an empty trace
        let frames = r.frames();
        assert_eq!(frames[0].h1, Some(2_000));
        assert_eq!(frames[0].h0, None);
        assert_eq!(frames[0].h2, None);
        assert_eq!(frames[1].h1, None);
        assert_eq!(frames[1].seq, 6);
    }

    #[test]
    fn reversed_stamps_clamp_and_summaries_skip_missing() {
        let a = FrameTrace {
            h2c: Some(5_000),
            h3: Some(4_000),
            h0: Some(1),
            h1: Some(2),
            ..FrameTrace::default()
        };
        assert_eq!(a.stage("H2c->H3"), Some(0));
        assert_eq!(a.stage("nope"), None);
        let rows = summarize_stages([a.clone(), FrameTrace::default()].iter());
        assert_eq!(rows["H0->H1"].n, 1);
        assert!(!rows.contains_key("H3->H4"));
    }
}
