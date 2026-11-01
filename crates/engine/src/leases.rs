//! Claude: the slots of a program endpoint, each held under an ID of its
//! own. Whoever withdraws a slot second finds it gone rather than removing
//! whatever took the slot over since, and an endpoint that ends withdraws
//! every slot still held at once and takes no more.

use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::rt_slots::RtSlots;

pub struct Leases<T, const N: usize> {
    slots: RtSlots<T, N>,
    held: Mutex<Held>,
}

#[derive(Default)]
struct Held {
    ended: bool,
    next: u64,
    by_id: Vec<(u64, usize)>,
}

impl<T, const N: usize> Leases<T, N> {
    pub fn new() -> Self {
        Self {
            slots: RtSlots::new(),
            held: Mutex::default(),
        }
    }

    fn held(&self) -> MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Claude: None once the endpoint ended or with every slot held.
    pub fn lease(&self, item: Box<T>) -> Option<u64> {
        let mut held = self.held();
        if held.ended {
            return None;
        }
        let slot = self.slots.insert(item).ok()?;
        let id = held.next;
        held.next += 1;
        held.by_id.push((id, slot));
        Some(id)
    }

    /// Claude: returns only after any reader still holding the item let go.
    pub fn withdraw(&self, id: u64) -> Option<Box<T>> {
        let mut held = self.held();
        let at = held.by_id.iter().position(|(held, _)| *held == id)?;
        let (_, slot) = held.by_id.swap_remove(at);
        self.slots.remove(slot)
    }

    /// Claude: withdraws every slot held and refuses any lease after.
    pub fn end(&self) -> Vec<Box<T>> {
        let mut held = self.held();
        held.ended = true;
        held.by_id
            .drain(..)
            .filter_map(|(_, slot)| self.slots.remove(slot))
            .collect()
    }

    pub fn ended(&self) -> bool {
        self.held().ended
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// # Safety
    ///
    /// Claude: at most one thread may be inside `for_each` at a time.
    pub unsafe fn for_each(&self, visit: impl FnMut(&mut T)) {
        unsafe { self.slots.for_each(visit) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(leases: &Leases<u32, 2>) -> Vec<u32> {
        let mut seen = Vec::new();
        unsafe { leases.for_each(|item| seen.push(*item)) };
        seen.sort_unstable();
        seen
    }

    #[test]
    fn a_withdrawn_lease_never_removes_the_slot_taken_over_since() {
        let leases = Leases::<u32, 2>::new();
        let first = leases.lease(Box::new(1)).unwrap();
        assert_eq!(leases.withdraw(first).as_deref(), Some(&1));
        let second = leases.lease(Box::new(2)).unwrap();
        assert!(leases.withdraw(first).is_none());
        assert_eq!(items(&leases), [2]);
        assert_eq!(leases.withdraw(second).as_deref(), Some(&2));
    }

    #[test]
    fn an_end_withdraws_every_lease_and_refuses_new_ones() {
        let leases = Leases::<u32, 2>::new();
        let a = leases.lease(Box::new(1)).unwrap();
        leases.lease(Box::new(2)).unwrap();
        assert!(leases.lease(Box::new(3)).is_none(), "more than its slots");
        let mut ended: Vec<u32> = leases.end().into_iter().map(|item| *item).collect();
        ended.sort_unstable();
        assert_eq!(ended, [1, 2]);
        assert!(leases.ended() && leases.is_empty());
        assert!(leases.withdraw(a).is_none());
        assert!(leases.lease(Box::new(4)).is_none());
    }
}
