//! On-disk config file shared between the HAL plugin and the daemon.
//!
//! The plugin is the runtime source of truth for which virtual devices exist.
//! The daemon owns writes to this file; the plugin only reads it and watches
//! for changes. Schema is versioned so future fields (format variants,
//! per-device flags) can be added without breaking older readers.
//!
//! Both [`DEVICE_CONFIG_PATH`] and [`FALLBACK_DEVICE_CONFIG_PATH`] are
//! well-known locations. The plugin probes the Library path first and falls
//! back to `/tmp/` if it cannot read from Library (sandbox, permission
//! denied, missing directory, etc.).

use serde::{Deserialize, Serialize};

/// Primary on-disk location for the device config.
pub const DEVICE_CONFIG_PATH: &str = "/Library/Application Support/Spaghetti/devices.json";

/// Fallback location used when the primary path is unreadable — typically
/// because `coreaudiod`'s sandbox denies access to
/// `/Library/Application Support/`.
pub const FALLBACK_DEVICE_CONFIG_PATH: &str = "/tmp/spaghetti.devices.json";

/// Current schema version. Bump when a breaking change is introduced.
pub const DEVICE_CONFIG_VERSION: u32 = 1;

/// Top-level on-disk shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfigFile {
    pub version: u32,
    pub devices: Vec<DeviceConfigEntry>,
}

/// One virtual device entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfigEntry {
    /// Stable UID (e.g. `"SpaghettiDevice_UID"` or `"spaghetti_virtual_<uuid>"`).
    pub uid: String,
    /// Human-readable name shown in AudioMIDI Setup.
    pub name: String,
    /// Channels per frame (typically 2).
    pub channels: u32,
    /// Sample rate in Hz (typically 48000).
    pub sample_rate: u32,
}

impl DeviceConfigFile {
    /// Empty-but-versioned file.
    pub fn empty() -> Self {
        Self {
            version: DEVICE_CONFIG_VERSION,
            devices: Vec::new(),
        }
    }
}

impl Default for DeviceConfigFile {
    fn default() -> Self {
        Self::empty()
    }
}
