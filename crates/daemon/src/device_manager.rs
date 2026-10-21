//! File-backed virtual device registry.
//!
//! The HAL plugin owns runtime truth about which virtual Core Audio devices
//! exist. The daemon drives the plugin by rewriting a JSON config file at a
//! well-known path; the plugin watches that file and republishes its
//! `DeviceList` whenever the contents change.
//!
//! `DeviceManager` here is just the write side of that contract: it owns an
//! in-memory mirror of the on-disk config and performs atomic
//! write-temp-then-rename persistence on every mutation. Creating a device
//! appends an entry and polls [`audio::find_device_by_uid`] until the plugin
//! picks up the change; destroying removes the entry and waits for the
//! CoreAudio device to disappear.
//!
//! The config file is shared with the HAL plugin (see
//! [`spaghetti_common::device_config`]) so path and schema constants live in
//! the common crate.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

use tokio::time::Instant;

use spaghetti_common::device_config::{
    DEVICE_CONFIG_PATH, DEVICE_CONFIG_VERSION, DeviceConfigEntry, DeviceConfigFile,
    FALLBACK_DEVICE_CONFIG_PATH,
};
use spaghetti_common::ipc_protocol::DeviceInfo;

/// UID of the built-in loopback device baked into the HAL plugin. This entry
/// is always present in the config and cannot be destroyed.
const SPAGHETTI_DEVICE_UID: &str = "SpaghettiDevice_UID";
const SPAGHETTI_DEVICE_NAME: &str = "Spaghetti";
const DEFAULT_CHANNELS: u32 = 2;
const DEFAULT_SAMPLE_RATE: u32 = 48000;

const DEVICE_APPEAR_TIMEOUT: Duration = Duration::from_secs(10);
const DEVICE_DISAPPEAR_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval while waiting for the plugin.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// File-persistence layer for virtual device definitions.
pub struct DeviceManager {
    /// Absolute path chosen at construction. Either [`DEVICE_CONFIG_PATH`] or
    /// [`FALLBACK_DEVICE_CONFIG_PATH`].
    config_path: PathBuf,
    /// In-memory mirror of the on-disk config. Mutations update this and then
    /// persist atomically.
    file: RwLock<DeviceConfigFile>,
    /// Function used to probe whether a device UID has materialized in
    /// CoreAudio. Indirected for testability.
    lookup: DeviceLookup,
}

/// Pluggable device-existence probe. Production wiring uses
/// [`audio::find_device_by_uid`]; tests substitute a fake.
type DeviceLookup = Box<dyn Fn(&str) -> bool + Send + Sync>;

impl DeviceManager {
    /// Construct a new manager, choosing the best available config path and
    /// bootstrapping the file with a default entry if it does not exist yet.
    pub fn new() -> Self {
        Self::with_lookup(Box::new(|uid| crate::audio::find_device_by_uid(uid).is_some()))
    }

    fn with_lookup(lookup: DeviceLookup) -> Self {
        let config_path = pick_config_path();
        tracing::info!(path = %config_path.display(), "device config path selected");

        let file = match load_config(&config_path) {
            Ok(Some(mut cfg)) => {
                tracing::info!(
                    devices = cfg.devices.len(),
                    "loaded existing device config"
                );
                if !cfg.devices.iter().any(|d| d.uid == SPAGHETTI_DEVICE_UID) {
                    cfg.devices.insert(0, default_entry());
                    if let Err(e) = persist(&config_path, &cfg) {
                        tracing::warn!(error = %e, "failed to restore default device entry");
                    }
                }
                cfg
            }
            Ok(None) => {
                let cfg = default_config();
                if let Err(e) = persist(&config_path, &cfg) {
                    tracing::warn!(error = %e, "failed to write initial device config");
                } else {
                    tracing::info!("wrote initial device config");
                }
                cfg
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to read device config; starting fresh");
                let cfg = default_config();
                if let Err(e) = persist(&config_path, &cfg) {
                    tracing::warn!(error = %e, "failed to write initial device config");
                }
                cfg
            }
        };

        Self {
            config_path,
            file: RwLock::new(file),
            lookup,
        }
    }

    /// Append a new device entry, persist it, and wait for the plugin to
    /// publish it. On timeout the change is rolled back.
    pub async fn create_device(
        &self,
        name: &str,
        channels: Option<u32>,
    ) -> Result<DeviceInfo, String> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err("device name must not be empty".into());
        }

        let channels = channels.unwrap_or(DEFAULT_CHANNELS);
        if channels == 0 {
            return Err("device must have at least one channel".into());
        }

        let uid = format!("spaghetti_virtual_{}", uuid::Uuid::new_v4().simple());
        let entry = DeviceConfigEntry {
            uid: uid.clone(),
            name: trimmed.to_string(),
            channels,
            sample_rate: DEFAULT_SAMPLE_RATE,
        };

        {
            let mut cfg = self.write_lock();
            cfg.devices.push(entry.clone());
            persist(&self.config_path, &cfg)
                .map_err(|e| format!("persist device config: {e}"))?;
        }

        tracing::info!(uid = %uid, name = %trimmed, channels, "added virtual device to config");

        match self.wait_for_device(&uid, true, DEVICE_APPEAR_TIMEOUT).await {
            Ok(()) => Ok(entry_to_info(&entry)),
            Err(timeout_err) => {
                tracing::warn!(uid = %uid, "plugin did not materialize device in time, rolling back");
                if let Err(rollback_err) = self.remove_entry(&uid) {
                    tracing::error!(
                        uid = %uid,
                        error = %rollback_err,
                        "failed to roll back device config entry"
                    );
                }
                Err(timeout_err)
            }
        }
    }

    /// Remove a device entry if it is neither the built-in default nor a
    /// live route target.
    ///
    /// Claude: callers must hold the lock that guards `in_use_routes` until
    /// this returns, or a route can be created to a device being removed.
    pub fn remove_device(&self, uid: &str, in_use_routes: &[u32]) -> Result<(), String> {
        if uid == SPAGHETTI_DEVICE_UID {
            return Err("cannot destroy the built-in Spaghetti device".into());
        }

        if !in_use_routes.is_empty() {
            let ids = in_use_routes
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "device {uid} is in use by route(s) {ids} — remove them first"
            ));
        }

        self.remove_entry(uid)?;
        tracing::info!(uid = %uid, "removed virtual device from config");
        Ok(())
    }

    pub async fn wait_until_removed(&self, uid: &str) {
        if let Err(e) = self
            .wait_for_device(uid, false, DEVICE_DISAPPEAR_TIMEOUT)
            .await
        {
            tracing::warn!(uid = %uid, error = %e, "plugin did not retract device in time");
        }
    }

    /// Snapshot the in-memory config as a list of [`DeviceInfo`] for IPC.
    pub fn list_devices(&self) -> Vec<DeviceInfo> {
        let cfg = self.read_lock();
        cfg.devices.iter().map(entry_to_info).collect()
    }

    /// Convenience — expose the resolved config path for logging/diagnostics.
    #[allow(dead_code)]
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    fn remove_entry(&self, uid: &str) -> Result<(), String> {
        let mut cfg = self.write_lock();
        let before = cfg.devices.len();
        cfg.devices.retain(|d| d.uid != uid);
        if cfg.devices.len() == before {
            return Err(format!("device '{uid}' not found"));
        }
        persist(&self.config_path, &cfg).map_err(|e| format!("persist device config: {e}"))
    }

    async fn wait_for_device(
        &self,
        uid: &str,
        want_present: bool,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            let present = (self.lookup)(uid);
            if present == want_present {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(if want_present {
                    format!("timed out waiting for device '{uid}' to appear")
                } else {
                    format!("timed out waiting for device '{uid}' to disappear")
                });
            }
            tokio::time::sleep(DEVICE_POLL_INTERVAL).await;
        }
    }

    fn read_lock(&self) -> std::sync::RwLockReadGuard<'_, DeviceConfigFile> {
        match self.file.read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn write_lock(&self) -> std::sync::RwLockWriteGuard<'_, DeviceConfigFile> {
        match self.file.write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Claude: the daemon runs as the logged-in user, so the Library path only
/// works because `scripts/install.sh` creates its directory owned by that
/// user. The `/tmp` fallback is wiped on reboot.
fn pick_config_path() -> PathBuf {
    let library_path = PathBuf::from(DEVICE_CONFIG_PATH);
    match check_writable(&library_path) {
        Ok(()) => library_path,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "cannot use Library device config path, falling back to /tmp (devices will not survive a reboot)"
            );
            PathBuf::from(FALLBACK_DEVICE_CONFIG_PATH)
        }
    }
}

fn check_writable(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let probe = temp_path(path);
    fs::File::create(&probe).map_err(|e| format!("create {}: {e}", probe.display()))?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

fn default_config() -> DeviceConfigFile {
    DeviceConfigFile {
        version: DEVICE_CONFIG_VERSION,
        devices: vec![default_entry()],
    }
}

fn default_entry() -> DeviceConfigEntry {
    DeviceConfigEntry {
        uid: SPAGHETTI_DEVICE_UID.to_string(),
        name: SPAGHETTI_DEVICE_NAME.to_string(),
        channels: DEFAULT_CHANNELS,
        sample_rate: DEFAULT_SAMPLE_RATE,
    }
}

fn entry_to_info(e: &DeviceConfigEntry) -> DeviceInfo {
    DeviceInfo {
        uid: e.uid.clone(),
        name: e.name.clone(),
        channels: e.channels,
        is_default: e.uid == SPAGHETTI_DEVICE_UID,
    }
}

fn load_config(path: &Path) -> Result<Option<DeviceConfigFile>, String> {
    match fs::read(path) {
        Ok(bytes) => {
            let cfg: DeviceConfigFile = serde_json::from_slice(&bytes)
                .map_err(|e| format!("parse {}: {e}", path.display()))?;
            if cfg.version != DEVICE_CONFIG_VERSION {
                return Err(format!(
                    "unsupported device config version {} (expected {})",
                    cfg.version, DEVICE_CONFIG_VERSION
                ));
            }
            Ok(Some(cfg))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

/// Write-temp-then-rename: the plugin's mtime-poll watcher may catch the file
/// mid-write and fail to parse partial JSON. Renaming on the same filesystem
/// is atomic on macOS (APFS) and ensures readers see a valid snapshot.
fn persist(path: &Path, cfg: &DeviceConfigFile) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(cfg)
        .map_err(|e| format!("serialise device config: {e}"))?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {e}", parent.display()))?;
    }

    let tmp = temp_path(path);
    {
        let mut f = fs::File::create(&tmp)
            .map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(&bytes)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, path)
        .map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), path.display()))?;
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    fn fake_lookup(flag: Arc<AtomicBool>) -> DeviceLookup {
        Box::new(move |_uid| flag.load(Ordering::Relaxed))
    }

    fn mgr_with_path(tmp: &Path, lookup: DeviceLookup) -> DeviceManager {
        // Bootstrap the file ourselves so we can place it at `tmp`.
        let cfg = default_config();
        persist(tmp, &cfg).unwrap();
        DeviceManager {
            config_path: tmp.to_path_buf(),
            file: RwLock::new(cfg),
            lookup,
        }
    }

    #[tokio::test]
    async fn create_fails_with_empty_name() {
        let dir = tempdir_path("create-empty");
        let path = dir.join("devices.json");
        let found = Arc::new(AtomicBool::new(true));
        let mgr = mgr_with_path(&path, fake_lookup(Arc::clone(&found)));

        let err = mgr.create_device("   ", None).await.unwrap_err();
        assert!(err.contains("empty"));
    }

    #[tokio::test]
    async fn create_persists_when_device_appears() {
        let dir = tempdir_path("create-ok");
        let path = dir.join("devices.json");
        let found = Arc::new(AtomicBool::new(true));
        let mgr = mgr_with_path(&path, fake_lookup(Arc::clone(&found)));

        let info = mgr.create_device("Stream", Some(2)).await.unwrap();
        assert!(info.uid.starts_with("spaghetti_virtual_"));
        let on_disk = load_config(&path).unwrap().unwrap();
        assert_eq!(on_disk.devices.len(), 2);
        assert_eq!(on_disk.devices[1].uid, info.uid);
    }

    #[tokio::test(start_paused = true)]
    async fn create_rolls_back_on_timeout() {
        let dir = tempdir_path("create-timeout");
        let path = dir.join("devices.json");
        let found = Arc::new(AtomicBool::new(false));
        let mgr = mgr_with_path(&path, fake_lookup(Arc::clone(&found)));

        let err = mgr.create_device("Ghost", Some(2)).await.unwrap_err();
        assert!(err.contains("timed out"));
        // Rollback: only the default remains.
        let listed = mgr.list_devices();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].uid, SPAGHETTI_DEVICE_UID);
        assert_eq!(load_config(&path).unwrap().unwrap().devices.len(), 1);
    }

    #[test]
    fn remove_refuses_builtin() {
        let dir = tempdir_path("destroy-builtin");
        let path = dir.join("devices.json");
        let found = Arc::new(AtomicBool::new(true));
        let mgr = mgr_with_path(&path, fake_lookup(Arc::clone(&found)));

        let err = mgr.remove_device(SPAGHETTI_DEVICE_UID, &[]).unwrap_err();
        assert!(err.contains("built-in"));
    }

    #[test]
    fn remove_refuses_in_use() {
        let dir = tempdir_path("destroy-in-use");
        let path = dir.join("devices.json");
        let found = Arc::new(AtomicBool::new(true));
        let mgr = mgr_with_path(&path, fake_lookup(Arc::clone(&found)));

        // Add an entry manually, then try to destroy with an active route.
        let entry = DeviceConfigEntry {
            uid: "spaghetti_virtual_test".into(),
            name: "Test".into(),
            channels: 2,
            sample_rate: 48000,
        };
        {
            let mut cfg = mgr.write_lock();
            cfg.devices.push(entry.clone());
            persist(&mgr.config_path, &cfg).unwrap();
        }

        let err = mgr
            .remove_device("spaghetti_virtual_test", &[7, 9])
            .unwrap_err();
        assert!(err.contains("in use"));
        assert!(err.contains("7"));
        assert!(err.contains("9"));

        mgr.remove_device("spaghetti_virtual_test", &[]).unwrap();
        assert_eq!(load_config(&path).unwrap().unwrap().devices.len(), 1);
    }

    #[test]
    fn list_marks_default() {
        let dir = tempdir_path("list-default");
        let path = dir.join("devices.json");
        let found = Arc::new(AtomicBool::new(true));
        let mgr = mgr_with_path(&path, fake_lookup(found));

        let devices = mgr.list_devices();
        assert_eq!(devices.len(), 1);
        assert!(devices[0].is_default);
        assert_eq!(devices[0].uid, SPAGHETTI_DEVICE_UID);
    }

    fn tempdir_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "spaghetti-devmgr-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
