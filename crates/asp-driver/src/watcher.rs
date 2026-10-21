//! Config-file watcher.
//!
//! A single background thread polls both the primary and fallback config
//! paths every [`POLL_INTERVAL`]. When a file appears or its mtime moves
//! forward, the watcher parses it and swaps a rebuilt [`DeviceRegistry`]
//! into the `ArcSwap`. After every successful swap it notifies the HAL host
//! via `PropertiesChanged` so Core Audio re-queries the device list.
//!
//! Errors on read/parse never propagate and never swap: the current registry
//! stays live, and the next tick tries again.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::thread;
use std::time::{Duration, SystemTime};

use spaghetti_common::device_config::{
    DEVICE_CONFIG_PATH, DEVICE_CONFIG_VERSION, DeviceConfigFile, FALLBACK_DEVICE_CONFIG_PATH,
};

use crate::log;
use crate::registry;

/// Poll period for mtime checks. Chosen to balance UI responsiveness against
/// wasted syscalls; the whole point of `ArcSwap` is that reads on the IO path
/// are lock-free regardless of polling cadence.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

static START: Once = Once::new();

/// Claude: the initial build skips `PropertiesChanged`. The host reads the
/// device list after `Initialize` returns, and notifying from inside
/// `Initialize` triggers a coreaudiod probe cascade that can deadlock clients
/// making CoreAudio calls concurrently during plugin init.
pub fn start() {
    START.call_once(|| {
        let mut watch = WatchState::default();
        let entries = watch.poll().map(|file| file.devices).unwrap_or_default();
        let initial = registry::rebuild_from_entries(&registry::current(), &entries);
        log(&format!(
            "initial registry: {:?}",
            initial.devices().map(|d| d.uid.as_str()).collect::<Vec<_>>()
        ));
        registry::store(initial);

        thread::spawn(move || {
            loop {
                thread::sleep(POLL_INTERVAL);
                if let Some(file) = watch.poll() {
                    apply_config(&file);
                }
            }
        });
    });
}

#[derive(Default)]
struct WatchState {
    last: Option<(PathBuf, SystemTime)>,
    logged_missing: bool,
}

impl WatchState {
    fn poll(&mut self) -> Option<DeviceConfigFile> {
        let Some((path, mtime)) = pick_live_path() else {
            if self.last.is_some() && !self.logged_missing {
                log("config file disappeared — keeping last loaded registry");
                self.logged_missing = true;
            }
            return None;
        };
        self.logged_missing = false;

        if let Some((last_path, last_mtime)) = &self.last
            && *last_path == path
            && mtime <= *last_mtime
        {
            return None;
        }
        // Advance mtime anyway so we don't spam the same failure; the
        // next write (which bumps mtime again) will retry.
        self.last = Some((path.clone(), mtime));

        match try_read(&path) {
            Ok(Some(file)) => {
                log(&format!(
                    "config loaded from {} ({} devices)",
                    path.display(),
                    file.devices.len()
                ));
                Some(file)
            }
            Ok(None) => None,
            Err(e) => {
                log(&format!(
                    "config read failed at {}: {e} — keeping current registry",
                    path.display()
                ));
                None
            }
        }
    }
}

/// Read + parse one path. Returns `Ok(None)` if the file is absent, `Err` if
/// present but unreadable/unparseable.
fn try_read(path: &Path) -> Result<Option<DeviceConfigFile>, String> {
    match fs::read(path) {
        Ok(bytes) => {
            let parsed: DeviceConfigFile =
                serde_json::from_slice(&bytes).map_err(|e| format!("parse failed: {e}"))?;
            Ok(Some(parsed))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Build a new registry snapshot from `file` and atomically swap it in, then
/// notify the host. Safe to call from any non-RT thread.
fn apply_config(file: &DeviceConfigFile) {
    if file.version != DEVICE_CONFIG_VERSION {
        log(&format!(
            "config version {} does not match expected {DEVICE_CONFIG_VERSION} — loading anyway",
            file.version
        ));
    }

    let previous = registry::current();
    let new = registry::rebuild_from_entries(&previous, &file.devices);

    let added: Vec<String> = new
        .by_uid
        .keys()
        .filter(|uid| !previous.by_uid.contains_key(uid.as_str()))
        .cloned()
        .collect();
    let removed: Vec<String> = previous
        .by_uid
        .keys()
        .filter(|uid| !new.by_uid.contains_key(uid.as_str()))
        .cloned()
        .collect();

    drop(previous);
    registry::store(new);

    if added.is_empty() && removed.is_empty() {
        return;
    }
    log(&format!("registry swapped — added={added:?} removed={removed:?}"));
    crate::driver::notify_device_list_changed();
}

/// Return the first candidate path that currently exists, with its mtime.
fn pick_live_path() -> Option<(PathBuf, SystemTime)> {
    [DEVICE_CONFIG_PATH, FALLBACK_DEVICE_CONFIG_PATH]
        .into_iter()
        .map(PathBuf::from)
        .find_map(|path| {
            let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok()?;
            Some((path, mtime))
        })
}
