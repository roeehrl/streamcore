//! Two real touch contacts for a two-finger gesture: when they go down, move, are re-sent and
//! lift. The injection itself (one `InjectSyntheticPointerInput` call with both contacts, IDs 0
//! and 1, so apps see an ordinary pinch, rotate or pan) is behind a closure, so the rules are
//! tested on every platform.

use crate::pen::{
    POINTER_FLAG_CANCELED, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE,
    POINTER_FLAG_UP, POINTER_FLAG_UPDATE,
};

/// Windows cancels a contact that gets no input for about a second, so held fingers are re-sent
/// this often.
pub const REFRESH_MS: u64 = 40;
/// A gesture the client never ends is lifted after this long without an event.
pub const WATCHDOG_MS: u64 = 2000;

/// The two contacts, relative to the top-left of the virtual screen.
pub type Contacts = [(i32, i32); 2];

const HELD: u32 = POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;

#[derive(Debug, Default)]
pub struct TouchSession {
    down: bool,
    contacts: Contacts,
    last_inject: u64,
    last_event: u64,
}

impl TouchSession {
    pub fn is_down(&self) -> bool {
        self.down
    }

    /// Fingers down (or moved, if already down). `inject` posts both contacts with the given
    /// pointer flags and says whether Windows took them. False when it did not; the contacts are
    /// then lifted.
    pub fn touch(
        &mut self,
        contacts: Contacts,
        now: u64,
        inject: &mut dyn FnMut(u32, Contacts) -> bool,
    ) -> bool {
        self.contacts = contacts;
        self.last_event = now;
        self.last_inject = now;
        let phase = if self.down {
            POINTER_FLAG_UPDATE
        } else {
            POINTER_FLAG_DOWN
        };
        if inject(HELD | phase, contacts) {
            self.down = true;
            return true;
        }
        if self.down {
            inject(POINTER_FLAG_UP | POINTER_FLAG_CANCELED, contacts);
            self.down = false;
        }
        false
    }

    /// Fingers up. `cancel` marks the contacts as cancelled rather than lifted.
    pub fn end(&mut self, cancel: bool, inject: &mut dyn FnMut(u32, Contacts) -> bool) {
        if !self.down {
            return;
        }
        let cancelled = if cancel { POINTER_FLAG_CANCELED } else { 0 };
        inject(POINTER_FLAG_UP | cancelled, self.contacts);
        self.down = false;
    }

    /// When [`tick`](Self::tick) next has something to do.
    pub fn next_deadline(&self) -> Option<u64> {
        self.down
            .then(|| self.last_inject.saturating_add(REFRESH_MS))
    }

    /// Re-sends held contacts, or lifts them when the client has been silent too long.
    pub fn tick(&mut self, now: u64, inject: &mut dyn FnMut(u32, Contacts) -> bool) {
        if !self.down {
            return;
        }
        if now.saturating_sub(self.last_event) > WATCHDOG_MS {
            self.end(true, inject);
        } else if now.saturating_sub(self.last_inject) >= REFRESH_MS {
            self.last_inject = now;
            inject(HELD | POINTER_FLAG_UPDATE, self.contacts);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Contacts = [(10, 10), (30, 10)];
    const B: Contacts = [(5, 10), (35, 10)];

    #[test]
    fn down_update_and_up() {
        let mut log = Vec::new();
        let mut inject = |flags, contacts| {
            log.push((flags, contacts));
            true
        };
        let mut touch = TouchSession::default();
        assert!(touch.touch(A, 0, &mut inject));
        assert!(touch.touch(B, 10, &mut inject));
        touch.end(false, &mut inject);
        touch.end(false, &mut inject);
        assert!(!touch.is_down());
        assert_eq!(
            log,
            [
                (HELD | POINTER_FLAG_DOWN, A),
                (HELD | POINTER_FLAG_UPDATE, B),
                (POINTER_FLAG_UP, B)
            ]
        );
    }

    #[test]
    fn held_contacts_are_refreshed_and_a_silent_gesture_is_lifted() {
        let mut log = Vec::new();
        let mut inject = |flags, _| {
            log.push(flags);
            true
        };
        let mut touch = TouchSession::default();
        touch.touch(A, 0, &mut inject);
        assert_eq!(touch.next_deadline(), Some(40));
        touch.tick(20, &mut inject);
        touch.tick(40, &mut inject);
        assert_eq!(touch.next_deadline(), Some(80));
        touch.tick(2001, &mut inject);
        assert!(!touch.is_down());
        assert_eq!(touch.next_deadline(), None);
        touch.tick(2100, &mut inject);
        assert_eq!(
            log,
            [
                HELD | POINTER_FLAG_DOWN,
                HELD | POINTER_FLAG_UPDATE,
                POINTER_FLAG_UP | POINTER_FLAG_CANCELED
            ]
        );
    }

    #[test]
    fn a_failed_injection_lifts_what_was_down() {
        let mut ok = true;
        let mut log = Vec::new();
        let mut touch = TouchSession::default();
        assert!(touch.touch(A, 0, &mut |flags, _| {
            log.push(flags);
            ok
        }));
        ok = false;
        assert!(!touch.touch(B, 5, &mut |flags, _| {
            log.push(flags);
            ok
        }));
        assert!(!touch.is_down());
        assert_eq!(
            log,
            [
                HELD | POINTER_FLAG_DOWN,
                HELD | POINTER_FLAG_UPDATE,
                POINTER_FLAG_UP | POINTER_FLAG_CANCELED
            ]
        );
        // Never down: a failure has nothing to lift.
        let mut count = 0;
        assert!(!TouchSession::default().touch(A, 0, &mut |_, _| {
            count += 1;
            false
        }));
        assert_eq!(count, 1);
    }

    #[test]
    fn cancel_marks_the_contacts_cancelled() {
        let mut log = Vec::new();
        let mut inject = |flags, _| {
            log.push(flags);
            true
        };
        let mut touch = TouchSession::default();
        touch.touch(A, 0, &mut inject);
        touch.end(true, &mut inject);
        assert_eq!(log[1], POINTER_FLAG_UP | POINTER_FLAG_CANCELED);
    }
}
