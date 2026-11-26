//! Claude: Core Audio holds up every client of a device for tens of
//! milliseconds after each level change a driver reports, the client setting
//! the volume included, so a held volume key steps at that pace rather than
//! its own. A change is reported once the level has stayed unchanged for
//! `SETTLE`, so the steps of one hold are reported together. The activity
//! region carries each change at once.

use std::collections::HashMap;
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, Once, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

const SETTLE: Duration = Duration::from_millis(60);

/// Claude: when each level control with an unreported change is due.
static DUE: LazyLock<Mutex<HashMap<u32, Instant>>> = LazyLock::new(Default::default);
static CHANGED: Condvar = Condvar::new();
static REPORTER: Once = Once::new();

fn due() -> MutexGuard<'static, HashMap<u32, Instant>> {
    DUE.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn report_level_change(control_id: u32) {
    due().insert(control_id, Instant::now() + SETTLE);
    CHANGED.notify_one();
    REPORTER.call_once(|| {
        let _ = thread::Builder::new()
            .name("level reports".into())
            .spawn(report_when_due);
    });
}

fn report_when_due() {
    let mut controls = due();
    loop {
        let now = Instant::now();
        let reported: Vec<u32> = controls
            .iter()
            .filter(|(_, due)| **due <= now)
            .map(|(control, _)| *control)
            .collect();
        if !reported.is_empty() {
            reported.iter().for_each(|control| {
                controls.remove(control);
            });
            drop(controls);
            reported
                .into_iter()
                .for_each(crate::driver::report_level_changed);
            controls = due();
            continue;
        }
        controls = match controls.values().min().copied() {
            Some(next) => {
                CHANGED
                    .wait_timeout(controls, next - now)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
            None => CHANGED
                .wait(controls)
                .unwrap_or_else(PoisonError::into_inner),
        };
    }
}
