//! Claude: a hook in every device lookup, for tests that need to see which
//! thread looks a device up, and to hold it there.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread::{self, ThreadId};
use std::time::Duration;

/// Claude: no device has this UID; looking it up stalls while `STALLED` is
/// set and records the thread it ran on.
pub(crate) const UID: &str = "spaghetti-test-stalled-lookup";
pub(crate) static STALLED: AtomicBool = AtomicBool::new(false);
static THREADS: Mutex<Vec<ThreadId>> = Mutex::new(Vec::new());

pub(crate) fn looking_up(uid: &str) {
    if uid != UID {
        return;
    }
    threads().push(thread::current().id());
    while STALLED.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(1));
    }
}

/// Claude: the threads that looked `UID` up since the last call.
pub(crate) fn take_threads() -> Vec<ThreadId> {
    std::mem::take(&mut *threads())
}

fn threads() -> std::sync::MutexGuard<'static, Vec<ThreadId>> {
    THREADS.lock().unwrap_or_else(PoisonError::into_inner)
}
