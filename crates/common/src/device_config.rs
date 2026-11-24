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
    /// What the device's volume and mute controls do to its audio.
    #[serde(default)]
    pub volume: Volume,
    /// Claude: the UID of a device whose output this device's input plays,
    /// in place of its own. That device must have the same channels and
    /// sample rate, or the input is silent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_from: Option<String>,
    /// Claude: hidden from device lists and from being the default device;
    /// a program still opens it by its UID.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

/// What a virtual device's volume and mute controls do to the audio written
/// to it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Volume {
    /// The device scales its audio by its volume and silences it when muted.
    #[default]
    Applied,
    /// The device passes its audio through at full scale and only reports
    /// its volume and mute, for a program that applies them elsewhere.
    Reported,
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
