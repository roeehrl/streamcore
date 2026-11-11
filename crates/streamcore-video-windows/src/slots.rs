//! Surface ids (design doc 3.2): an index and a generation into a native slot table, so a late
//! `release` of a reused slot is a no-op. Id 0 is never issued.

/// A table of values addressed by `SurfaceId`-style ids: low 32 bits are `index + 1`, high 32
/// bits the slot's generation.
pub struct SlotTable<T> {
    slots: Vec<(u32, Option<T>)>,
    free: Vec<usize>,
}

impl<T> Default for SlotTable<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }
}

fn split(id: u64) -> Option<(usize, u32)> {
    let index = (id & 0xffff_ffff) as usize;
    (index != 0).then(|| (index - 1, (id >> 32) as u32))
}

impl<T> SlotTable<T> {
    /// Stores `value` and returns its id (never 0).
    pub fn insert(&mut self, value: T) -> u64 {
        let index = match self.free.pop() {
            Some(i) => i,
            None => {
                self.slots.push((0, None));
                self.slots.len() - 1
            }
        };
        let slot = &mut self.slots[index];
        slot.0 = slot.0.wrapping_add(1);
        slot.1 = Some(value);
        (u64::from(slot.0) << 32) | (index as u64 + 1)
    }

    pub fn get(&self, id: u64) -> Option<&T> {
        let (index, generation) = split(id)?;
        match self.slots.get(index)? {
            (g, Some(value)) if *g == generation => Some(value),
            _ => None,
        }
    }

    /// Removes and returns the value; `None` for an unknown or stale id.
    pub fn remove(&mut self, id: u64) -> Option<T> {
        let (index, generation) = split(id)?;
        let slot = self.slots.get_mut(index)?;
        if slot.0 != generation || slot.1.is_none() {
            return None;
        }
        self.free.push(index);
        slot.1.take()
    }

    /// Live entries.
    pub fn len(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Removes every entry (stale ids stay stale).
    pub fn drain(&mut self) -> Vec<T> {
        let mut out = Vec::new();
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if let Some(value) = slot.1.take() {
                out.push(value);
                self.free.push(index);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_nonzero_and_generational() {
        let mut t = SlotTable::default();
        let a = t.insert("a");
        assert_ne!(a, 0);
        assert_eq!(t.get(a), Some(&"a"));
        assert_eq!(t.remove(a), Some("a"));
        // The slot is reused with a new generation: the old id is stale.
        let b = t.insert("b");
        assert_ne!(a, b);
        assert_eq!(a & 0xffff_ffff, b & 0xffff_ffff);
        assert_eq!(t.get(a), None);
        assert_eq!(t.remove(a), None);
        assert_eq!(t.get(b), Some(&"b"));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn zero_and_unknown_ids() {
        let mut t: SlotTable<u8> = SlotTable::default();
        assert_eq!(t.get(0), None);
        assert_eq!(t.remove(0), None);
        assert_eq!(t.remove(0x0000_0001_0000_0009), None);
        assert!(t.is_empty());
    }

    #[test]
    fn drain_empties() {
        let mut t = SlotTable::default();
        let ids: Vec<u64> = (0..4).map(|i| t.insert(i)).collect();
        let _ = t.remove(ids[1]);
        let mut left = t.drain();
        left.sort_unstable();
        assert_eq!(left, vec![0, 2, 3]);
        assert!(t.is_empty());
        assert!(ids.iter().all(|id| t.get(*id).is_none()));
    }
}
