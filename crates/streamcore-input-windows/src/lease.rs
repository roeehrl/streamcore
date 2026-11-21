//! Who may inject: one lease at a time, revocable. The state machine only; the injector wraps it
//! in a lock and ties revocation to releasing what is held.

/// The lease bookkeeping. A lease is its generation number; only the newest one, while not
/// revoked, is current.
#[derive(Debug, Default)]
pub struct LeaseTable {
    generation: u64,
    live: bool,
}

impl LeaseTable {
    /// Grants a new lease, or `None` while another one is live.
    pub fn acquire(&mut self) -> Option<u64> {
        if self.live {
            return None;
        }
        self.generation += 1;
        self.live = true;
        Some(self.generation)
    }

    /// Whether this lease may inject.
    pub fn is_current(&self, lease: u64) -> bool {
        self.live && lease == self.generation
    }

    /// Ends this lease. False when it was already stale, so a stale lease cannot end its
    /// successor.
    pub fn revoke(&mut self, lease: u64) -> bool {
        let current = self.is_current(lease);
        if current {
            self.live = false;
        }
        current
    }

    /// Ends whichever lease is live. False when none was.
    pub fn revoke_current(&mut self) -> bool {
        std::mem::take(&mut self.live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_lease_at_a_time() {
        let mut table = LeaseTable::default();
        let first = table.acquire().expect("nothing holds the lease");
        assert!(table.is_current(first));
        assert_eq!(table.acquire(), None);
        assert!(table.revoke(first));
        assert!(!table.is_current(first));
        let second = table.acquire().expect("the first lease was revoked");
        assert_ne!(first, second);
        assert!(table.is_current(second) && !table.is_current(first));
    }

    #[test]
    fn a_stale_lease_cannot_end_its_successor() {
        let mut table = LeaseTable::default();
        let first = table.acquire().expect("free");
        assert!(table.revoke_current());
        assert!(!table.revoke_current());
        let second = table.acquire().expect("free again");
        assert!(!table.revoke(first));
        assert!(table.is_current(second));
        assert!(table.revoke(second));
        assert!(!table.revoke(second));
    }

    #[test]
    fn nothing_is_current_before_the_first_lease() {
        let table = LeaseTable::default();
        assert!(!table.is_current(0));
        assert!(!table.is_current(1));
    }
}
