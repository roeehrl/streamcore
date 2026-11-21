//! The pace keys go out at, shared by typed text and shortcut taps so they stay in order in the
//! app.
//!
//! Apps that read keys through the Text Services Framework (the Windows 11 text editor among
//! them) handle a key when they get to it, not when it was sent. A burst of VK_PACKETs then all
//! read the character of the last one; a burst of layout keys loses Shift (it is already up
//! again) and drops keys while the app is busy after a word; and a shortcut that comes down and
//! up in one burst right after typed text overtakes the characters still waiting. Every key
//! therefore stays down [`HOLD_MS`] and the next one waits [`GAP_MS`] after the last came up: 50
//! characters a second, faster than anyone types.
//!
//! The pacer never sleeps. It says when a step may run, and the injection thread waits for that
//! time between other events, so a long text never delays a pen stroke or a release.

/// How long a tapped key stays down.
pub const HOLD_MS: u64 = 10;
/// How long the next key waits after the last one came up.
pub const GAP_MS: u64 = 10;

/// One paced step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    /// A key goes down: it waits the gap after the last key-up.
    Press,
    /// A tapped key comes up: it waits the hold after its press.
    TapRelease,
    /// A held key comes up, at once.
    Release,
}

#[derive(Debug, Default)]
pub struct KeyPacer {
    last_up: Option<u64>,
    last_press: Option<u64>,
}

impl KeyPacer {
    /// The earliest time (milliseconds) a step may run.
    pub fn ready_at(&self, pace: Pace, now: u64) -> u64 {
        let after = |since: Option<u64>, wait| since.map_or(now, |t| t.saturating_add(wait));
        match pace {
            Pace::Press => after(self.last_up, GAP_MS),
            Pace::TapRelease => after(self.last_press, HOLD_MS),
            Pace::Release => now,
        }
        .max(now)
    }

    /// A step ran at `now`.
    pub fn ran(&mut self, pace: Pace, now: u64) {
        match pace {
            Pace::Press => self.last_press = Some(now),
            Pace::TapRelease | Pace::Release => self.last_up = Some(now),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_key_goes_at_once() {
        let pacer = KeyPacer::default();
        assert_eq!(pacer.ready_at(Pace::Press, 1000), 1000);
    }

    #[test]
    fn a_tap_holds_and_the_next_key_waits_the_gap() {
        let mut pacer = KeyPacer::default();
        pacer.ran(Pace::Press, 1000);
        assert_eq!(pacer.ready_at(Pace::TapRelease, 1000), 1010);
        assert_eq!(pacer.ready_at(Pace::TapRelease, 1004), 1010);
        pacer.ran(Pace::TapRelease, 1011);
        assert_eq!(pacer.ready_at(Pace::Press, 1011), 1021);
        // Long after the last key nothing waits.
        assert_eq!(pacer.ready_at(Pace::Press, 5000), 5000);
    }

    #[test]
    fn a_held_key_waits_the_gap_going_down_and_comes_up_at_once() {
        let mut pacer = KeyPacer::default();
        pacer.ran(Pace::TapRelease, 100);
        assert_eq!(pacer.ready_at(Pace::Press, 103), 110);
        pacer.ran(Pace::Press, 110);
        assert_eq!(pacer.ready_at(Pace::Release, 111), 111);
        pacer.ran(Pace::Release, 111);
        assert_eq!(pacer.ready_at(Pace::Press, 111), 121);
    }
}
