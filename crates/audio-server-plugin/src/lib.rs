//! Claude: a Core Audio AudioServerPlugIn driver that publishes virtual
//! loopback devices: what a device plays comes back on its input.
//!
//! A driver bundle is a `cdylib` that describes itself with a [`Plugin`] and
//! exports the factory function its `Info.plist` names, which hands over to
//! [`create`]:
//!
//! ```ignore
//! static PLUGIN: Plugin = Plugin { manufacturer: "Example", /* ... */ };
//!
//! #[unsafe(no_mangle)]
//! pub unsafe extern "C" fn example_create(
//!     allocator: CFAllocatorRef,
//!     requested_type: CFUUIDRef,
//! ) -> *mut c_void {
//!     unsafe { audio_server_plugin::create(&PLUGIN, allocator, requested_type) }
//! }
//! ```
//!
//! Each bundle loads into `coreaudiod` as its own image, so drivers built on
//! this crate run side by side without sharing state.

mod devices;
pub(crate) mod driver;
mod iface;
pub(crate) mod registry;
mod storage;
mod watcher;

use std::ffi::c_void;
use std::sync::OnceLock;

use coreaudio_sys::{CFAllocatorRef, CFUUIDBytes, CFUUIDGetUUIDBytes, CFUUIDRef};

pub use spaghetti_common::device_config::Volume;

/// Claude: a driver's identity and the devices it publishes.
pub struct Plugin {
    /// Claude: the manufacturer the plug-in and its devices report.
    pub manufacturer: &'static str,
    /// Claude: the model UID its devices report.
    pub model_uid: &'static str,
    /// Claude: devices published for as long as the driver is loaded, ahead
    /// of any a config file lists. A config file entry with the same UID is
    /// ignored.
    pub devices: &'static [Device],
    /// Claude: device config files, in the format of
    /// `spaghetti_common::device_config`, that list further devices. The
    /// first one present is read at load and watched for changes; with none,
    /// only `devices` are published.
    pub config_paths: &'static [&'static str],
    /// Claude: a device whose output is reported in a shared memory region.
    pub activity: Option<Activity>,
    /// Claude: the file the driver appends its diagnostics to.
    pub log_path: Option<&'static str>,
}

/// Claude: a device published for as long as the driver is loaded.
pub struct Device {
    pub uid: &'static str,
    pub name: &'static str,
    pub channels: u32,
    pub sample_rate: u32,
    pub volume: Volume,
}

/// Claude: reports how much a device plays, and how loud, in a POSIX shared
/// memory region (`spaghetti_common::activity::ActivityRegion`) that other
/// processes map by name. Creating the region replaces any of that name.
pub struct Activity {
    pub device_uid: &'static str,
    pub shm_name: &'static str,
}

static PLUGIN: OnceLock<&'static Plugin> = OnceLock::new();

static NO_PLUGIN: Plugin = Plugin {
    manufacturer: "",
    model_uid: "",
    devices: &[],
    config_paths: &[],
    activity: None,
    log_path: None,
};

/// Claude: the plugin `create` was first called with.
pub(crate) fn plugin() -> &'static Plugin {
    PLUGIN.get().copied().unwrap_or(&NO_PLUGIN)
}

/// Append a line to the plugin's log file, if it has one. File I/O errors
/// are silently swallowed — logging must never crash the host.
pub(crate) fn log(msg: &str) {
    use std::io::Write;
    let Some(path) = plugin().log_path else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "[{}] {msg}", plugin().manufacturer);
    }
}

/// kAudioServerPlugInTypeUUID: 443ABAB8-E7B3-491A-B985-BEB9187030DB
const PLUGIN_TYPE_UUID: CFUUIDBytes = CFUUIDBytes {
    byte0: 0x44, byte1: 0x3A, byte2: 0xBA, byte3: 0xB8,
    byte4: 0xE7, byte5: 0xB3, byte6: 0x49, byte7: 0x1A,
    byte8: 0xB9, byte9: 0x85, byte10: 0xBE, byte11: 0xB9,
    byte12: 0x18, byte13: 0x70, byte14: 0x30, byte15: 0xDB,
};

fn uuid_bytes(u: &CFUUIDBytes) -> [u8; 16] {
    [
        u.byte0, u.byte1, u.byte2, u.byte3, u.byte4, u.byte5, u.byte6, u.byte7,
        u.byte8, u.byte9, u.byte10, u.byte11, u.byte12, u.byte13, u.byte14, u.byte15,
    ]
}

pub(crate) fn uuid_eq(a: &CFUUIDBytes, b: &CFUUIDBytes) -> bool {
    uuid_bytes(a) == uuid_bytes(b)
}

/// Claude: the body of a driver's CFPlugIn factory function. coreaudiod
/// calls the factory after loading the bundle, passing the UUID of the type
/// it wants; this returns the driver for `kAudioServerPlugInTypeUUID` and
/// null for any other. The driver serves the first `plugin` passed.
///
/// # Safety
///
/// `requested_type_uuid` must be a valid, non-null `CFUUIDRef` allocated by
/// Core Foundation. The caller (coreaudiod / CFPlugIn) always guarantees this.
pub unsafe fn create(
    plugin: &'static Plugin,
    _allocator: CFAllocatorRef,
    requested_type_uuid: CFUUIDRef,
) -> *mut c_void {
    let _ = PLUGIN.set(plugin);
    let requested = unsafe { CFUUIDGetUUIDBytes(requested_type_uuid) };
    let matched = uuid_eq(&requested, &PLUGIN_TYPE_UUID);
    log(&format!(
        "create: uuid={:02x}{:02x}{:02x}{:02x}-... matched_plugin_type={matched}",
        requested.byte0, requested.byte1, requested.byte2, requested.byte3
    ));
    if matched {
        iface::DRIVER_REF.0 as *mut c_void
    } else {
        std::ptr::null_mut()
    }
}
