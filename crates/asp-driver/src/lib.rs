//! Spaghetti's AudioServerPlugIn driver, loaded by coreaudiod.

#![cfg(target_os = "macos")]

use std::ffi::c_void;

use audio_server_plugin::{Activity, Device, Plugin, Volume};
use coreaudio_sys::{CFAllocatorRef, CFUUIDRef};
use spaghetti_common::activity::ACTIVITY_SHM_NAME;
use spaghetti_common::audio_types::{CHANNELS, SAMPLE_RATE};
use spaghetti_common::device_config::{DEVICE_CONFIG_PATH, FALLBACK_DEVICE_CONFIG_PATH};

/// UID of the always-present loopback device.
pub const DEFAULT_DEVICE_UID: &str = "SpaghettiDevice_UID";

static PLUGIN: Plugin = Plugin {
    manufacturer: "Spaghetti",
    model_uid: "SpaghettiModel_UID",
    devices: &[Device {
        uid: DEFAULT_DEVICE_UID,
        name: "Spaghetti",
        channels: CHANNELS,
        sample_rate: SAMPLE_RATE as u32,
        volume: Volume::Applied,
    }],
    config_paths: &[DEVICE_CONFIG_PATH, FALLBACK_DEVICE_CONFIG_PATH],
    activity: Some(Activity {
        device_uid: DEFAULT_DEVICE_UID,
        shm_name: ACTIVITY_SHM_NAME,
    }),
    log_path: Some("/tmp/spaghetti-driver.log"),
};

/// The CFPlugIn factory function `Info.plist` names.
///
/// # Safety
///
/// `requested_type_uuid` must be a valid, non-null `CFUUIDRef` allocated by
/// Core Foundation. The caller (coreaudiod / CFPlugIn) always guarantees this.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn spaghetti_create(
    allocator: CFAllocatorRef,
    requested_type_uuid: CFUUIDRef,
) -> *mut c_void {
    unsafe { audio_server_plugin::create(&PLUGIN, allocator, requested_type_uuid) }
}
