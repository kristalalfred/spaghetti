//! Claude: keeps each device's volume and mute in the host's storage, which
//! coreaudiod keeps per plug-in, so a device comes back as it was after
//! coreaudiod restarts.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::sync::{Condvar, Mutex, MutexGuard, Once, PoisonError};
use std::thread;
use std::time::Duration;

use core_foundation_sys::base::{CFGetTypeID, CFRelease};
use core_foundation_sys::number::{
    CFNumberCreate, CFNumberGetTypeID, CFNumberGetValue, CFNumberRef, kCFNumberFloat32Type,
};
use coreaudio_sys::AudioServerPlugInHostInterface;

use crate::devices::DeviceInstance;
use crate::driver::{DRIVER, cfstr};

/// Claude: how long a device's volume and mute stay unchanged before they are
/// stored. A write to the host's storage rewrites coreaudiod's settings file
/// and holds up clients reading the device for tens of milliseconds, so a
/// held volume key or a dragged slider stores once, after it settles.
const SETTLE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq)]
struct Level {
    volume: f32,
    muted: bool,
}

/// Claude: levels waiting to be stored, by device UID.
static PENDING: Mutex<BTreeMap<String, Level>> = Mutex::new(BTreeMap::new());
static CHANGED: Condvar = Condvar::new();
static WRITER: Once = Once::new();

fn host() -> Option<*mut AudioServerPlugInHostInterface> {
    let host = DRIVER.host.load(Ordering::SeqCst);
    (!host.is_null()).then_some(host)
}

fn pending() -> MutexGuard<'static, BTreeMap<String, Level>> {
    PENDING.lock().unwrap_or_else(PoisonError::into_inner)
}

fn keys(uid: &str) -> [String; 2] {
    [format!("{uid} volume"), format!("{uid} mute")]
}

/// Claude: stores the device's volume and mute once they settle, off the
/// calling thread. Never call from the IO path.
pub(crate) fn save(instance: &DeviceInstance) {
    let level = Level {
        volume: f32::from_bits(instance.volume.load(Ordering::Relaxed)),
        muted: instance.mute.load(Ordering::Relaxed) != 0,
    };
    pending().insert(instance.uid.clone(), level);
    CHANGED.notify_one();
    WRITER.call_once(|| {
        let _ = thread::Builder::new()
            .name("storage".into())
            .spawn(write_when_settled);
    });
}

fn write_when_settled() {
    loop {
        let mut levels = pending();
        while levels.is_empty() {
            levels = CHANGED.wait(levels).unwrap_or_else(PoisonError::into_inner);
        }
        loop {
            let (next, waited) = CHANGED
                .wait_timeout(levels, SETTLE)
                .unwrap_or_else(PoisonError::into_inner);
            levels = next;
            if waited.timed_out() {
                break;
            }
        }
        drop(levels);
        flush();
    }
}

/// Claude: stores every level waiting to be stored now.
fn flush() {
    let levels = std::mem::take(&mut *pending());
    let Some(host) = host() else {
        return;
    };
    for (uid, level) in levels {
        let values = [level.volume, f32::from(u8::from(level.muted))];
        for (key, value) in keys(&uid).iter().zip(values) {
            unsafe { write(host, key, value) };
        }
    }
}

unsafe fn write(host: *mut AudioServerPlugInHostInterface, key: &str, value: f32) {
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

/// Claude: brings back a device's volume and mute, the latest saved when one
/// is waiting to be stored; leaves the device as it is for anything not
/// stored.
pub(crate) fn restore(instance: &DeviceInstance) {
    let (volume, muted) = match pending().get(&instance.uid) {
        Some(level) => (Some(level.volume), Some(level.muted)),
        None => {
            let Some(host) = host() else {
                return;
            };
            let [volume, mute] = keys(&instance.uid).map(|key| unsafe { read(host, &key) });
            (volume, mute.map(|mute| mute != 0.0))
        }
    };
    if let Some(volume) = volume {
        let volume = volume.clamp(0.0, 1.0);
        instance.volume.store(volume.to_bits(), Ordering::Relaxed);
    }
    if let Some(muted) = muted {
        instance.mute.store(u32::from(muted), Ordering::Relaxed);
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
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    use core_foundation_sys::base::CFRetain;
    use core_foundation_sys::string::{CFStringGetCString, kCFStringEncodingUTF8};
    use coreaudio_sys::{
        AudioObjectID, AudioObjectPropertyAddress, AudioServerPlugInHostRef, CFPropertyListRef,
        CFStringRef, OSStatus,
    };
    use spaghetti_common::device_config::{DeviceConfigEntry, Volume};

    use super::*;

    struct Stored(*const c_void);
    unsafe impl Send for Stored {}

    static STORAGE: LazyLock<Mutex<HashMap<String, Stored>>> = LazyLock::new(Default::default);

    fn key_string(key: CFStringRef) -> String {
        let mut buffer = [0i8; 256];
        unsafe { CFStringGetCString(key.cast(), buffer.as_mut_ptr(), 256, kCFStringEncodingUTF8) };
        unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    unsafe extern "C" fn write(
        _host: AudioServerPlugInHostRef,
        key: CFStringRef,
        data: CFPropertyListRef,
    ) -> OSStatus {
        unsafe { CFRetain(data) };
        STORAGE
            .lock()
            .unwrap()
            .insert(key_string(key), Stored(data));
        0
    }

    unsafe extern "C" fn copy(
        _host: AudioServerPlugInHostRef,
        key: CFStringRef,
        out: *mut CFPropertyListRef,
    ) -> OSStatus {
        match STORAGE.lock().unwrap().get(&key_string(key)) {
            Some(stored) => {
                unsafe {
                    CFRetain(stored.0);
                    *out = stored.0;
                }
                0
            }
            None => 1,
        }
    }

    unsafe extern "C" fn changed(
        _host: AudioServerPlugInHostRef,
        _object: AudioObjectID,
        _count: u32,
        _addresses: *const AudioObjectPropertyAddress,
    ) -> OSStatus {
        0
    }

    static HOST: AudioServerPlugInHostInterface = AudioServerPlugInHostInterface {
        PropertiesChanged: Some(changed),
        CopyFromStorage: Some(copy),
        WriteToStorage: Some(write),
        DeleteFromStorage: None,
        RequestDeviceConfigurationChange: None,
    };

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
        DRIVER.host.store(
            (&HOST as *const AudioServerPlugInHostInterface).cast_mut(),
            Ordering::SeqCst,
        );
        let before = instance("kept");
        before.volume.store(0.3f32.to_bits(), Ordering::Relaxed);
        before.mute.store(1, Ordering::Relaxed);
        save(&before);
        flush();

        let after = instance("kept");
        restore(&after);
        assert_eq!(f32::from_bits(after.volume.load(Ordering::Relaxed)), 0.3);
        assert_eq!(after.mute.load(Ordering::Relaxed), 1);

        let other = instance("never stored");
        restore(&other);
        assert_eq!(f32::from_bits(other.volume.load(Ordering::Relaxed)), 1.0);
        assert_eq!(other.mute.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_volume_is_stored_once_it_settles_and_comes_back_meanwhile() {
        DRIVER.host.store(
            (&HOST as *const AudioServerPlugInHostInterface).cast_mut(),
            Ordering::SeqCst,
        );
        let device = instance("settling");
        for volume in [0.2f32, 0.4, 0.6] {
            device.volume.store(volume.to_bits(), Ordering::Relaxed);
            save(&device);
        }
        assert!(!STORAGE.lock().unwrap().contains_key("settling volume"));

        let meanwhile = instance("settling");
        restore(&meanwhile);
        assert_eq!(f32::from_bits(meanwhile.volume.load(Ordering::Relaxed)), 0.6);

        let started = std::time::Instant::now();
        while !STORAGE.lock().unwrap().contains_key("settling volume") {
            assert!(started.elapsed() < SETTLE * 4, "the volume was never stored");
            thread::sleep(Duration::from_millis(10));
        }
        let stored = instance("settling");
        restore(&stored);
        assert_eq!(f32::from_bits(stored.volume.load(Ordering::Relaxed)), 0.6);
    }
}
