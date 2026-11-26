//! Claude: a host for tests that keeps storage in memory and records the
//! property changes the driver reports. Tests share it, since the driver
//! holds one host.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::sync::{LazyLock, Mutex};

use core_foundation_sys::base::CFRetain;
use core_foundation_sys::string::{CFStringGetCString, kCFStringEncodingUTF8};
use coreaudio_sys::{
    AudioObjectID, AudioObjectPropertyAddress, AudioServerPlugInHostInterface,
    AudioServerPlugInHostRef, CFPropertyListRef, CFStringRef, OSStatus,
};

use crate::driver::DRIVER;

struct Stored(*const c_void);
unsafe impl Send for Stored {}

static STORAGE: LazyLock<Mutex<HashMap<String, Stored>>> = LazyLock::new(Default::default);
type Change = (AudioObjectID, Vec<u32>);

static CHANGES: LazyLock<Mutex<Vec<Change>>> = LazyLock::new(Default::default);

pub fn install() {
    DRIVER.host.store(
        (&HOST as *const AudioServerPlugInHostInterface).cast_mut(),
        Ordering::SeqCst,
    );
}

/// Claude: the selectors of each `PropertiesChanged` call on `object`, in order.
pub fn changes(object: AudioObjectID) -> Vec<Vec<u32>> {
    CHANGES
        .lock()
        .unwrap()
        .iter()
        .filter(|(changed, _)| *changed == object)
        .map(|(_, selectors)| selectors.clone())
        .collect()
}

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
    object: AudioObjectID,
    count: u32,
    addresses: *const AudioObjectPropertyAddress,
) -> OSStatus {
    let addresses = unsafe { std::slice::from_raw_parts(addresses, count as usize) };
    let selectors = addresses.iter().map(|address| address.mSelector).collect();
    CHANGES.lock().unwrap().push((object, selectors));
    0
}

static HOST: AudioServerPlugInHostInterface = AudioServerPlugInHostInterface {
    PropertiesChanged: Some(changed),
    CopyFromStorage: Some(copy),
    WriteToStorage: Some(write),
    DeleteFromStorage: None,
    RequestDeviceConfigurationChange: None,
};
