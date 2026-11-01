//! Claude: the slots of a program endpoint, each held under an ID of its
//! own. Whoever withdraws a slot second finds it gone rather than removing
//! whatever took the slot over since, and an endpoint that ends withdraws
//! every slot still held at once and takes no more.

use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::rt_slots::{Retired, RtSlots};

pub struct Leases<T, const N: usize> {
    slots: RtSlots<T, N>,
    held: Mutex<Held<T>>,
}

struct Held<T> {
    ended: bool,
    next: u64,
    by_id: Vec<(u64, usize)>,
    /// Claude: withdrawn items a reader pass may still hold, freed by a later
    /// call once it is over, or with the leases.
    retired: Vec<Retired<T>>,
}

impl<T, const N: usize> Leases<T, N> {
    pub fn new() -> Self {
        Self {
            slots: RtSlots::new(),
            held: Mutex::new(Held {
                ended: false,
                next: 0,
                by_id: Vec::new(),
                retired: Vec::new(),
            }),
        }
    }

    /// Claude: the held leases, after freeing what no reader holds any more.
    fn held(&self) -> MutexGuard<'_, Held<T>> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let retired = std::mem::take(&mut held.retired);
        for item in retired {
            if let Err(still) = self.slots.reclaim(item) {
                held.retired.push(still);
            }
        }
        held
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

    /// Claude: whether the lease was held. The reader visits the item no
    /// more once this returns, without waiting for a pass under way.
    pub fn withdraw(&self, id: u64) -> bool {
        let mut held = self.held();
        let Some(at) = held.by_id.iter().position(|(held, _)| *held == id) else {
            return false;
        };
        let (_, slot) = held.by_id.swap_remove(at);
        self.retire(&mut held, slot);
        true
    }

    /// Claude: withdraws every lease held, returning their IDs, and refuses
    /// any lease after.
    pub fn end(&self) -> Vec<u64> {
        let mut held = self.held();
        held.ended = true;
        let ended = std::mem::take(&mut held.by_id);
        for &(_, slot) in &ended {
            self.retire(&mut held, slot);
        }
        ended.into_iter().map(|(id, _)| id).collect()
    }

    fn retire(&self, held: &mut Held<T>, slot: usize) {
        if let Some(item) = self.slots.take(slot)
            && let Err(still) = self.slots.reclaim(item)
        {
            held.retired.push(still);
        }
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

impl<T, const N: usize> Drop for Leases<T, N> {
    fn drop(&mut self) {
        let held = self.held.get_mut().unwrap_or_else(PoisonError::into_inner);
        for item in held.retired.drain(..) {
            // Claude: dropping takes the leases exclusively, so no pass runs.
            drop(unsafe { item.into_box() });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

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
        assert!(leases.withdraw(first));
        assert!(items(&leases).is_empty());
        let second = leases.lease(Box::new(2)).unwrap();
        assert!(!leases.withdraw(first));
        assert_eq!(items(&leases), [2]);
        assert!(leases.withdraw(second));
    }

    #[test]
    fn an_end_withdraws_every_lease_and_refuses_new_ones() {
        let leases = Leases::<u32, 2>::new();
        let a = leases.lease(Box::new(1)).unwrap();
        let b = leases.lease(Box::new(2)).unwrap();
        assert!(leases.lease(Box::new(3)).is_none(), "more than its slots");
        let mut ended = leases.end();
        ended.sort_unstable();
        assert_eq!(ended, [a, b]);
        assert!(leases.ended() && leases.is_empty());
        assert!(!leases.withdraw(a));
        assert!(leases.lease(Box::new(4)).is_none());
    }

    /// Claude: counts its copies alive.
    struct Counted(Arc<AtomicUsize>);

    impl Counted {
        fn new(alive: &Arc<AtomicUsize>) -> Box<Self> {
            alive.fetch_add(1, Ordering::SeqCst);
            Box::new(Self(Arc::clone(alive)))
        }
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Claude: a reader parked inside its pass, as a real-time thread the
    /// system stopped would be, until the returned sender is used.
    fn park(leases: &Arc<Leases<Counted, 4>>) -> (mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let reader = {
            let leases = Arc::clone(leases);
            std::thread::spawn(move || unsafe {
                let mut once = Some((entered_tx, release_rx));
                leases.for_each(|_| {
                    if let Some((entered, release)) = once.take() {
                        entered.send(()).unwrap();
                        let _ = release.recv();
                    }
                });
            })
        };
        entered_rx.recv().unwrap();
        (release_tx, reader)
    }

    #[test]
    fn withdrawing_or_ending_never_waits_for_a_reader_parked_in_its_pass() {
        let alive = Arc::new(AtomicUsize::new(0));
        let leases = Arc::new(Leases::<Counted, 4>::new());
        let first = leases.lease(Counted::new(&alive)).unwrap();
        leases.lease(Counted::new(&alive)).unwrap();
        let (release, reader) = park(&leases);
        let (done, watched) = mpsc::channel::<()>();
        let watchdog = {
            let release = release.clone();
            std::thread::spawn(move || {
                if watched.recv_timeout(Duration::from_secs(2)).is_err() {
                    let _ = release.send(());
                }
            })
        };

        let started = Instant::now();
        assert!(leases.withdraw(first));
        assert!(leases.lease(Counted::new(&alive)).is_some());
        assert_eq!(leases.end().len(), 2);
        let took = started.elapsed();
        let _ = done.send(());
        watchdog.join().unwrap();
        assert!(leases.ended() && leases.is_empty());
        assert!(took < Duration::from_millis(100), "{took:?}");
        assert_eq!(
            alive.load(Ordering::SeqCst),
            3,
            "freed while a pass may hold them"
        );

        let _ = release.send(());
        reader.join().unwrap();
        assert!(leases.ended());
        assert_eq!(alive.load(Ordering::SeqCst), 0, "freed by the next call");
    }

    #[test]
    fn items_retired_under_a_parked_reader_are_freed_with_the_leases() {
        let alive = Arc::new(AtomicUsize::new(0));
        let leases = Arc::new(Leases::<Counted, 4>::new());
        leases.lease(Counted::new(&alive)).unwrap();
        let (release, reader) = park(&leases);
        leases.end();
        release.send(()).unwrap();
        reader.join().unwrap();
        assert_eq!(alive.load(Ordering::SeqCst), 1);
        drop(Arc::into_inner(leases).unwrap());
        assert_eq!(alive.load(Ordering::SeqCst), 0);
    }
}
