//! Claude: spaghetti's virtual audio driver: the virtual devices it publishes
//! and its activity. The driver exists only on macOS; elsewhere every request
//! for it is refused.

use spaghetti_common::Volume;
use spaghetti_common::ipc_protocol::{ActivityInfo, DeviceInfo};

pub use imp::Driver;

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::Mutex;

    use spaghetti_engine::{ActivityReader, DeviceManager, FpsTracker};

    use super::*;

    /// Freshness window for the driver's last WriteMix timestamp — older than this
    /// and the activity snapshot is considered stale (driver idle or gone).
    const ACTIVITY_FRESHNESS_NS: u64 = 200_000_000;

    pub struct Driver {
        devices: DeviceManager,
        activity: ActivityState,
    }

    /// Per-server (not per-connection) activity tracking, so the FPS window
    /// stays consistent across concurrent clients.
    ///
    /// `reader` is behind a `Mutex` so the lazy-attach path (first successful open
    /// after daemon starts before the driver has loaded) can update it safely.
    struct ActivityState {
        reader: Mutex<Option<ActivityReader>>,
        fps: FpsTracker,
    }

    impl Driver {
        pub fn open() -> Self {
            // Bootstrap the device config file before anything else. On a fresh
            // install this writes the default entry so the HAL plugin can publish
            // `SpaghettiDevice_UID` when it loads.
            let devices = DeviceManager::new();

            // Activity shm is a soft dependency — daemon keeps running if the driver's
            // shm region is unavailable (reads will just return defaults).
            let reader = match ActivityReader::open(spaghetti_common::ACTIVITY_SHM_NAME) {
                Ok(r) => {
                    tracing::info!(
                        "activity shm opened: {}",
                        spaghetti_common::ACTIVITY_SHM_NAME
                    );
                    Some(r)
                }
                Err(e) => {
                    tracing::warn!("activity shm unavailable: {e}");
                    None
                }
            };

            Self {
                devices,
                activity: ActivityState {
                    reader: Mutex::new(reader),
                    fps: FpsTracker::new(),
                },
            }
        }

        /// Claude: logs whether the driver is installed, from a blocking task,
        /// since the lookup waits on coreaudiod.
        pub fn look_for_device(&self) {
            tokio::task::spawn_blocking(|| {
                match spaghetti_engine::find_device_by_uid("SpaghettiDevice_UID") {
                    Some(id) => tracing::info!("Found Spaghetti device: object_id={id}"),
                    None => tracing::warn!(
                        "Spaghetti virtual device not found — is the driver installed and coreaudiod loaded it?"
                    ),
                }
            });
        }

        pub async fn create_device(
            &self,
            name: &str,
            channels: Option<u32>,
            volume: Volume,
        ) -> Result<String, String> {
            let info = self.devices.create_device(name, channels, volume).await?;
            Ok(info.uid)
        }

        /// Claude: callers must hold the lock that guards `in_use_routes` until
        /// this returns, or a route can be created to a device being removed.
        pub fn remove_device(&self, uid: &str, in_use_routes: &[u32]) -> Result<(), String> {
            self.devices.remove_device(uid, in_use_routes)
        }

        pub async fn wait_until_removed(&self, uid: &str) {
            self.devices.wait_until_removed(uid).await;
        }

        pub fn list_devices(&self) -> Result<Vec<DeviceInfo>, String> {
            Ok(self.devices.list_devices())
        }

        /// Build an [`ActivityInfo`] from the driver's shm snapshot.
        ///
        /// Returns zeroed / inactive values when the shm region is unavailable.
        /// If the region was not available at daemon start, attempts a lazy open on
        /// each call — the driver may have loaded after the daemon started.
        pub fn activity(&self) -> Result<ActivityInfo, String> {
            let activity = &self.activity;
            let mut guard = match activity.reader.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };

            // Lazy attach: try opening now if we don't have a reader yet.
            if guard.is_none() {
                match ActivityReader::open(spaghetti_common::ACTIVITY_SHM_NAME) {
                    Ok(r) => {
                        tracing::info!("activity shm attached (lazy)");
                        *guard = Some(r);
                    }
                    Err(_) => {
                        return Ok(ActivityInfo {
                            active: false,
                            frames_per_second: 0,
                            peak_db: -96.0,
                            io_running: false,
                            total_frames: 0,
                        });
                    }
                }
            }

            let reader = guard.as_ref().expect("reader was just set");
            let snap = reader.snapshot();
            let fps = activity.fps.update(snap.frames_written);
            let stale = reader.is_stale(snap.last_write_host_time, ACTIVITY_FRESHNESS_NS);

            let peak_db = if snap.peak > 0.0 {
                let db = 20.0 * snap.peak.log10();
                if db < -96.0 { -96.0 } else { db }
            } else {
                -96.0
            };

            Ok(ActivityInfo {
                active: snap.io_running && !stale,
                frames_per_second: fps,
                peak_db,
                io_running: snap.io_running,
                total_frames: snap.frames_written,
            })
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    const ABSENT: &str = "virtual devices and their activity need spaghetti's audio driver, \
                          which exists only on macOS";

    pub struct Driver;

    impl Driver {
        pub fn open() -> Self {
            Self
        }

        pub fn look_for_device(&self) {}

        pub async fn create_device(
            &self,
            _name: &str,
            _channels: Option<u32>,
            _volume: Volume,
        ) -> Result<String, String> {
            Err(ABSENT.into())
        }

        pub fn remove_device(&self, _uid: &str, _in_use_routes: &[u32]) -> Result<(), String> {
            Err(ABSENT.into())
        }

        pub async fn wait_until_removed(&self, _uid: &str) {}

        pub fn list_devices(&self) -> Result<Vec<DeviceInfo>, String> {
            Err(ABSENT.into())
        }

        pub fn activity(&self) -> Result<ActivityInfo, String> {
            Err(ABSENT.into())
        }
    }
}
