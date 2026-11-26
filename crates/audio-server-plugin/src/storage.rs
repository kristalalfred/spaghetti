//! Claude: keeps each device's volume and mute in the host's storage, which
//! coreaudiod keeps per plug-in, so a device comes back as it was after
//! coreaudiod restarts.

use std::ffi::c_void;
use std::sync::atomic::Ordering;

use core_foundation_sys::base::{CFGetTypeID, CFRelease};
use core_foundation_sys::number::{
    CFNumberCreate, CFNumberGetTypeID, CFNumberGetValue, CFNumberRef, kCFNumberFloat32Type,
};
use coreaudio_sys::AudioServerPlugInHostInterface;

use crate::devices::DeviceInstance;
use crate::driver::{DRIVER, cfstr};

fn host() -> Option<*mut AudioServerPlugInHostInterface> {
    let host = DRIVER.host.load(Ordering::SeqCst);
    (!host.is_null()).then_some(host)
}

fn keys(instance: &DeviceInstance) -> [String; 2] {
    [
        format!("{} volume", instance.uid),
        format!("{} mute", instance.uid),
    ]
}

/// Claude: stores the device's volume and mute. Never call from the IO path.
pub(crate) fn save(instance: &DeviceInstance) {
    let Some(host) = host() else {
        return;
    };
    let volume = f32::from_bits(instance.volume.load(Ordering::Relaxed));
    let muted = instance.mute.load(Ordering::Relaxed) as f32;
    for (key, value) in keys(instance).iter().zip([volume, muted]) {
        let key = cfstr(key);
        let number = unsafe {
            CFNumberCreate(
                std::ptr::null(),
                kCFNumberFloat32Type,
                (&value as *const f32).cast(),
            )
        };
        if !key.is_null() && !number.is_null() {
            unsafe {
                if let Some(write) = (*host).WriteToStorage {
                    let _ = write(host, key, number as *const c_void);
                }
            }
        }
        unsafe {
            if !number.is_null() {
                CFRelease(number.cast());
            }
            if !key.is_null() {
                CFRelease(key.cast());
            }
        }
    }
}

/// Claude: brings back a stored volume and mute; leaves the device as it is
/// for anything not stored.
pub(crate) fn restore(instance: &DeviceInstance) {
    let Some(host) = host() else {
        return;
    };
    let [volume, mute] = keys(instance).map(|key| unsafe { read(host, &key) });
    if let Some(volume) = volume {
        let volume = volume.clamp(0.0, 1.0);
        instance.volume.store(volume.to_bits(), Ordering::Relaxed);
    }
    if let Some(muted) = mute {
        instance.mute.store(u32::from(muted != 0.0), Ordering::Relaxed);
    }
}

unsafe fn read(host: *mut AudioServerPlugInHostInterface, key: &str) -> Option<f32> {
    let copy = unsafe { (*host).CopyFromStorage }?;
    let key = cfstr(key);
    if key.is_null() {
        return None;
    }
    let mut data: *const c_void = std::ptr::null();
    let status = unsafe { copy(host, key, &mut data) };
    unsafe { CFRelease(key.cast()) };
    if status != 0 || data.is_null() {
        return None;
    }
    let mut value = 0f32;
    let read = unsafe {
        CFGetTypeID(data) == CFNumberGetTypeID()
            && CFNumberGetValue(
                data as CFNumberRef,
                kCFNumberFloat32Type,
                (&mut value as *mut f32).cast(),
            )
    };
    unsafe { CFRelease(data) };
    read.then_some(value)
}

#[cfg(test)]
mod tests {
    use spaghetti_common::device_config::{DeviceConfigEntry, Volume};

    use super::*;

    fn instance(uid: &str) -> DeviceInstance {
        let entry = DeviceConfigEntry {
            uid: uid.into(),
            name: uid.into(),
            channels: 2,
            sample_rate: 48_000,
            volume: Volume::Reported,
            input_from: None,
            hidden: false,
        };
        DeviceInstance::new(100, 101, 102, 103, 104, &entry, 16)
    }

    #[test]
    fn a_device_comes_back_with_its_stored_volume_and_mute() {
        crate::test_host::install();
        let before = instance("kept");
        before.volume.store(0.3f32.to_bits(), Ordering::Relaxed);
        before.mute.store(1, Ordering::Relaxed);
        save(&before);

        let after = instance("kept");
        restore(&after);
        assert_eq!(f32::from_bits(after.volume.load(Ordering::Relaxed)), 0.3);
        assert_eq!(after.mute.load(Ordering::Relaxed), 1);

        let other = instance("never stored");
        restore(&other);
        assert_eq!(f32::from_bits(other.volume.load(Ordering::Relaxed)), 1.0);
        assert_eq!(other.mute.load(Ordering::Relaxed), 0);
    }
}
