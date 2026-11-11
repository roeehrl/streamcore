//! Bookkeeping for an asynchronous Media Foundation encoder MFT (design doc 4.2), kept apart from
//! the COM calls so it can be tested anywhere.
//!
//! An async MFT may be fed only after `METransformNeedInput`; each event is one input credit. The
//! stream's `submit` never blocks, so an input that arrives without a credit is held, one deep,
//! and fed by the event thread when the next `NeedInput` arrives (`StreamEncoder` has no "ready
//! for input" event to make Rust retry). Outputs carry the input's sample time, so each output
//! finds its frame; inputs ahead of it that produced no output are reported as dropped. The
//! counters give `min_inputs_before_output` and the deepest the encoder ever got.

use std::collections::VecDeque;

/// What `submit` decided.
#[derive(Debug, PartialEq, Eq)]
pub enum Accept<S> {
    /// A credit was taken: call `ProcessInput` with this input now.
    Now(S),
    /// No credit: held until the next `NeedInput`.
    Held,
    /// The session failed (`fail`): refused, the input and metadata come back.
    Failed(S),
    /// No credit and an input is already held: refuse (`SubmitResult::Busy`).
    Busy(S),
}

/// Inputs and outputs of one MFT session. `S` is what `ProcessInput` takes (a sample), `M` is
/// the per-frame metadata kept until the frame's output or drop.
pub struct MftBook<S, M> {
    credits: u32,
    held: Option<(i64, S, M)>,
    in_encoder: VecDeque<(i64, M)>,
    inputs: u64,
    outputs: u64,
    inputs_before_first_output: Option<u64>,
    max_in_encoder: usize,
    /// Set by `fail`: every later `submit` is refused, under the same lock that takes the frames.
    failed: bool,
}

impl<S, M> Default for MftBook<S, M> {
    fn default() -> Self {
        Self {
            credits: 0,
            held: None,
            in_encoder: VecDeque::new(),
            inputs: 0,
            outputs: 0,
            inputs_before_first_output: None,
            max_in_encoder: 0,
            failed: false,
        }
    }
}

impl<S, M> MftBook<S, M> {
    /// A new input with sample time `time` (100 ns units, strictly increasing).
    pub fn submit(&mut self, time: i64, input: S, meta: M) -> (Accept<S>, Option<M>) {
        if self.failed {
            return (Accept::Failed(input), Some(meta));
        }
        if self.held.is_some() {
            return (Accept::Busy(input), Some(meta));
        }
        if self.credits > 0 {
            self.credits -= 1;
            self.in_encoder.push_back((time, meta));
            return (Accept::Now(input), None);
        }
        self.held = Some((time, input, meta));
        (Accept::Held, None)
    }

    /// `METransformNeedInput`: one more credit. Returns the held input if there was one; its
    /// metadata moves to the in-encoder list and the credit is used.
    pub fn need_input(&mut self) -> Option<S> {
        self.credits += 1;
        let (time, input, meta) = self.held.take()?;
        self.credits -= 1;
        self.in_encoder.push_back((time, meta));
        Some(input)
    }

    /// `ProcessInput` accepted the input with sample time `time`.
    pub fn input_accepted(&mut self) {
        self.inputs += 1;
        self.max_in_encoder = self.max_in_encoder.max(self.in_encoder.len());
    }

    /// `ProcessInput` failed for `time`: its metadata comes back (the frame is dropped). With
    /// `keep_credit` the `NeedInput` credit it took is given back: the MFT did not take the
    /// input, so it will not send another `NeedInput` for that slot.
    pub fn input_failed(&mut self, time: i64, keep_credit: bool) -> Option<M> {
        let at = self.in_encoder.iter().rposition(|(t, _)| *t == time)?;
        if keep_credit {
            self.credits += 1;
        }
        self.in_encoder.remove(at).map(|(_, m)| m)
    }

    /// An output with sample time `time`. Returns its frame's metadata and the metadata of every
    /// earlier input that produced no output. An unknown time returns `(None, [])`.
    pub fn output(&mut self, time: i64) -> (Option<M>, Vec<M>) {
        let Some(at) = self.in_encoder.iter().position(|(t, _)| *t == time) else {
            return (None, Vec::new());
        };
        let skipped: Vec<M> = self.in_encoder.drain(..at).map(|(_, m)| m).collect();
        let meta = self.in_encoder.pop_front().map(|(_, m)| m);
        self.outputs += 1;
        if self.inputs_before_first_output.is_none() {
            self.inputs_before_first_output = Some(self.inputs);
        }
        (meta, skipped)
    }

    /// The session failed: refuses every later `submit` and returns every frame still held or in
    /// the encoder, oldest first. `None` if it had already failed (the frames went then).
    pub fn fail(&mut self) -> Option<Vec<M>> {
        if self.failed {
            return None;
        }
        self.failed = true;
        Some(self.take_all())
    }

    pub fn is_failed(&self) -> bool {
        self.failed
    }

    /// Flush, drain end or shutdown: every frame still held or in the encoder, oldest first.
    pub fn take_all(&mut self) -> Vec<M> {
        let mut out: Vec<M> = self.in_encoder.drain(..).map(|(_, m)| m).collect();
        if let Some((_, _, m)) = self.held.take() {
            out.push(m);
        }
        out
    }

    pub fn credits(&self) -> u32 {
        self.credits
    }

    pub fn in_encoder(&self) -> usize {
        self.in_encoder.len()
    }

    pub fn inputs(&self) -> u64 {
        self.inputs
    }

    pub fn outputs(&self) -> u64 {
        self.outputs
    }

    /// Inputs the encoder had accepted when its first output arrived.
    pub fn inputs_before_first_output(&self) -> Option<u64> {
        self.inputs_before_first_output
    }

    /// The most frames the encoder held at once.
    pub fn max_in_encoder(&self) -> usize {
        self.max_in_encoder
    }
}

/// The stream's permit target for an encoder (design doc 3.3): `max(min_inputs_before_output, 1)`
/// up to 3.
pub fn permit_target(min_inputs_before_output: Option<u64>) -> u8 {
    min_inputs_before_output.unwrap_or(1).clamp(1, 3) as u8
}

/// What a session failure reports, from `MftBook::fail`'s result (`taken`, read under the book's
/// lock) and `first` (a frame already taken out of the book): the frames to drop, `first` and then
/// the oldest, and whether this is the session's first failure. Only the first failure reports
/// `session_failed`, once, after its drops; a later one (`taken` is `None`) only drops `first`.
pub fn failure_report<M>(first: Option<M>, taken: Option<Vec<M>>) -> (Vec<M>, bool) {
    let first_failure = taken.is_some();
    let frames = first.into_iter().chain(taken.unwrap_or_default()).collect();
    (frames, first_failure)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credits_gate_inputs() {
        let mut b: MftBook<&str, u64> = MftBook::default();
        // No NeedInput yet: the first input is held, the second refused.
        assert_eq!(b.submit(10, "s1", 1), (Accept::Held, None));
        assert_eq!(b.submit(20, "s2", 2), (Accept::Busy("s2"), Some(2)));
        // NeedInput feeds the held input.
        assert_eq!(b.need_input(), Some("s1"));
        b.input_accepted();
        assert_eq!(b.credits(), 0);
        assert_eq!(b.in_encoder(), 1);
        // A credit with nothing held is kept for the next submit.
        assert_eq!(b.need_input(), None);
        assert_eq!(b.submit(30, "s3", 3), (Accept::Now("s3"), None));
        b.input_accepted();
        assert_eq!(b.max_in_encoder(), 2);
    }

    #[test]
    fn outputs_match_by_time_and_report_skips() {
        let mut b: MftBook<(), u64> = MftBook::default();
        for (t, m) in [(10, 1), (20, 2), (30, 3)] {
            let _ = b.need_input();
            assert_eq!(b.submit(t, (), m), (Accept::Now(()), None));
            b.input_accepted();
        }
        assert_eq!(b.output(99), (None, vec![]));
        assert_eq!(b.output(20), (Some(2), vec![1]));
        assert_eq!(b.inputs_before_first_output(), Some(3));
        assert_eq!(b.output(30), (Some(3), vec![]));
        assert_eq!(b.outputs(), 2);
        assert_eq!(b.in_encoder(), 0);
    }

    #[test]
    fn failed_input_and_take_all() {
        let mut b: MftBook<(), u64> = MftBook::default();
        let _ = b.need_input();
        let _ = b.submit(10, (), 1);
        assert_eq!(b.input_failed(10, false), Some(1));
        assert_eq!(b.input_failed(10, false), None);
        assert_eq!(b.credits(), 0);
        let _ = b.need_input();
        let _ = b.submit(20, (), 2);
        b.input_accepted();
        let _ = b.submit(30, (), 3); // held
        assert_eq!(b.take_all(), vec![2, 3]);
        assert_eq!(b.take_all(), Vec::<u64>::new());
    }

    #[test]
    fn failed_input_keeps_its_credit() {
        let mut b: MftBook<&str, u64> = MftBook::default();
        let _ = b.need_input();
        assert_eq!(b.submit(10, "s1", 1), (Accept::Now("s1"), None));
        assert_eq!(b.credits(), 0);
        // A non-fatal ProcessInput failure gives the credit back...
        assert_eq!(b.input_failed(10, true), Some(1));
        assert_eq!(b.credits(), 1);
        // ...so the next input goes in at once instead of waiting for a NeedInput that never
        // comes.
        assert_eq!(b.submit(20, "s2", 2), (Accept::Now("s2"), None));
        b.input_accepted();
        // An unknown time changes nothing.
        assert_eq!(b.input_failed(99, true), None);
        assert_eq!(b.credits(), 0);
        // A held input fed on NeedInput that fails keeps that credit too.
        assert_eq!(b.submit(30, "s3", 3), (Accept::Held, None));
        assert_eq!(b.need_input(), Some("s3"));
        assert_eq!(b.input_failed(30, true), Some(3));
        assert_eq!(b.credits(), 1);
        assert_eq!(b.take_all(), vec![2]);
    }

    #[test]
    fn permit_targets() {
        assert_eq!(permit_target(None), 1);
        assert_eq!(permit_target(Some(0)), 1);
        assert_eq!(permit_target(Some(2)), 2);
        assert_eq!(permit_target(Some(9)), 3);
    }

    #[test]
    fn fail_refuses_later_submits_and_takes_frames_once() {
        let mut b: MftBook<&str, u32> = MftBook::default();
        b.need_input();
        assert_eq!(b.submit(10, "s1", 1), (Accept::Now("s1"), None));
        assert_eq!(b.submit(20, "s2", 2), (Accept::Held, None));
        assert_eq!(b.fail(), Some(vec![1, 2]));
        assert!(b.is_failed());
        assert_eq!(b.fail(), None);
        assert_eq!(b.submit(30, "s3", 3), (Accept::Failed("s3"), Some(3)));
        assert_eq!(b.need_input(), None);
    }

    #[test]
    fn a_failure_reports_the_session_once_with_or_without_frames() {
        // Frames in flight: dropped, and the session reported.
        let mut b: MftBook<&str, u32> = MftBook::default();
        b.need_input();
        assert_eq!(b.submit(10, "s1", 1), (Accept::Now("s1"), None));
        assert_eq!(failure_report(None, b.fail()), (vec![1], true));
        // Nothing in flight: still reported, once.
        let mut b: MftBook<&str, u32> = MftBook::default();
        assert_eq!(failure_report(None, b.fail()), (vec![], true));
        assert_eq!(failure_report(None, b.fail()), (vec![], false));
        // The device-lost feed path: its frame first, then the session, once.
        let mut b: MftBook<&str, u32> = MftBook::default();
        b.need_input();
        b.need_input();
        assert_eq!(b.submit(10, "s1", 1), (Accept::Now("s1"), None));
        assert_eq!(b.submit(20, "s2", 2), (Accept::Now("s2"), None));
        let first = b.input_failed(20, false);
        assert_eq!(failure_report(first, b.fail()), (vec![2, 1], true));
        // A later feed failure on the failed session drops its frame only.
        assert_eq!(failure_report(Some(3), b.fail()), (vec![3], false));
    }

    #[test]
    fn input_failed_before_fail_keeps_the_frame_out_of_take_all() {
        let mut b: MftBook<&str, u32> = MftBook::default();
        b.need_input();
        assert_eq!(b.submit(10, "s1", 1), (Accept::Now("s1"), None));
        assert_eq!(b.input_failed(10, false), Some(1));
        assert_eq!(b.fail(), Some(vec![]));
    }
}
