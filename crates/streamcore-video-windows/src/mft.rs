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
        }
    }
}

impl<S, M> MftBook<S, M> {
    /// A new input with sample time `time` (100 ns units, strictly increasing).
    pub fn submit(&mut self, time: i64, input: S, meta: M) -> (Accept<S>, Option<M>) {
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

    /// `ProcessInput` failed for `time`: its metadata comes back (the frame is dropped).
    pub fn input_failed(&mut self, time: i64) -> Option<M> {
        let at = self.in_encoder.iter().rposition(|(t, _)| *t == time)?;
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
        assert_eq!(b.input_failed(10), Some(1));
        assert_eq!(b.input_failed(10), None);
        let _ = b.need_input();
        let _ = b.submit(20, (), 2);
        b.input_accepted();
        let _ = b.submit(30, (), 3); // held
        assert_eq!(b.take_all(), vec![2, 3]);
        assert_eq!(b.take_all(), Vec::<u64>::new());
    }

    #[test]
    fn permit_targets() {
        assert_eq!(permit_target(None), 1);
        assert_eq!(permit_target(Some(0)), 1);
        assert_eq!(permit_target(Some(2)), 2);
        assert_eq!(permit_target(Some(9)), 3);
    }
}
