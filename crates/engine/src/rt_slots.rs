use std::marker::PhantomData;
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// Claude: a fixed set of items shared with a single real-time reader. Writers
/// insert and remove items without blocking the reader, and the reader visits
/// items without locking or allocating.
pub struct RtSlots<T, const N: usize> {
    slots: [AtomicPtr<T>; N],
    /// Claude: odd while the reader is inside `for_each`.
    passes: AtomicUsize,
    _owns: PhantomData<*mut T>,
}

// Claude: items move from writers to the reader thread and only one thread
// touches an item at a time, as with `Mutex<T>`.
unsafe impl<T: Send, const N: usize> Send for RtSlots<T, N> {}
unsafe impl<T: Send, const N: usize> Sync for RtSlots<T, N> {}

impl<T, const N: usize> RtSlots<T, N> {
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            passes: AtomicUsize::new(0),
            _owns: PhantomData,
        }
    }

    pub fn insert(&self, item: Box<T>) -> Result<usize, Box<T>> {
        let item = Box::into_raw(item);
        for (index, slot) in self.slots.iter().enumerate() {
            if slot
                .compare_exchange(ptr::null_mut(), item, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(index);
            }
        }
        Err(unsafe { Box::from_raw(item) })
    }

    /// Claude: returns only after any reader pass that may still hold the item has finished.
    pub fn remove(&self, index: usize) -> Option<Box<T>> {
        let item = self.slots.get(index)?.swap(ptr::null_mut(), Ordering::SeqCst);
        if item.is_null() {
            return None;
        }
        let observed = self.passes.load(Ordering::SeqCst);
        if observed % 2 == 1 {
            while self.passes.load(Ordering::SeqCst) == observed {
                std::thread::yield_now();
            }
        }
        Some(unsafe { Box::from_raw(item) })
    }

    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(|slot| slot.load(Ordering::SeqCst).is_null())
    }

    /// # Safety
    ///
    /// Claude: at most one thread may be inside `for_each` at a time.
    pub unsafe fn for_each(&self, mut visit: impl FnMut(&mut T)) {
        self.passes.fetch_add(1, Ordering::SeqCst);
        for slot in &self.slots {
            let item = slot.load(Ordering::SeqCst);
            if !item.is_null() {
                visit(unsafe { &mut *item });
            }
        }
        self.passes.fetch_add(1, Ordering::SeqCst);
    }
}

impl<T, const N: usize> Drop for RtSlots<T, N> {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            let item = *slot.get_mut();
            if !item.is_null() {
                drop(unsafe { Box::from_raw(item) });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    fn visited(slots: &RtSlots<u32, 4>) -> Vec<u32> {
        let mut seen = Vec::new();
        unsafe { slots.for_each(|item| seen.push(*item)) };
        seen
    }

    #[test]
    fn visits_items_until_removed() {
        let slots = RtSlots::<u32, 4>::new();
        let a = slots.insert(Box::new(1)).unwrap();
        let b = slots.insert(Box::new(2)).unwrap();
        assert_eq!(visited(&slots), vec![1, 2]);

        assert_eq!(slots.remove(a).as_deref(), Some(&1));
        assert_eq!(visited(&slots), vec![2]);
        assert!(slots.remove(a).is_none());

        assert_eq!(slots.remove(b).as_deref(), Some(&2));
        assert!(slots.is_empty());
    }

    #[test]
    fn reuses_freed_slots_and_refuses_when_full() {
        let slots = RtSlots::<u32, 4>::new();
        for i in 0..4 {
            slots.insert(Box::new(i)).unwrap();
        }
        assert_eq!(slots.insert(Box::new(9)).unwrap_err(), Box::new(9));

        slots.remove(2).unwrap();
        assert_eq!(slots.insert(Box::new(9)), Ok(2));
    }

    #[test]
    fn drop_releases_remaining_items() {
        let tracker = Arc::new(());
        let slots = RtSlots::<Arc<()>, 4>::new();
        slots.insert(Box::new(Arc::clone(&tracker))).unwrap();
        slots.insert(Box::new(Arc::clone(&tracker))).unwrap();
        assert_eq!(Arc::strong_count(&tracker), 3);
        drop(slots);
        assert_eq!(Arc::strong_count(&tracker), 1);
    }

    #[test]
    fn remove_waits_for_the_reader_to_leave() {
        let slots = Arc::new(RtSlots::<u32, 4>::new());
        let index = slots.insert(Box::new(7)).unwrap();

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let reader = {
            let slots = Arc::clone(&slots);
            std::thread::spawn(move || unsafe {
                slots.for_each(|_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                });
            })
        };
        entered_rx.recv().unwrap();

        let removed = Arc::new(AtomicBool::new(false));
        let remover = {
            let slots = Arc::clone(&slots);
            let removed = Arc::clone(&removed);
            std::thread::spawn(move || {
                let item = slots.remove(index);
                removed.store(true, Ordering::SeqCst);
                item
            })
        };

        std::thread::sleep(Duration::from_millis(50));
        assert!(!removed.load(Ordering::SeqCst));
        assert!(slots.is_empty());

        release_tx.send(()).unwrap();
        reader.join().unwrap();
        assert_eq!(remover.join().unwrap().as_deref(), Some(&7));
    }
}
