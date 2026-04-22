use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_foundation_sys::string::CFStringRef;
use coreaudio_sys::*;
use std::mem;
use std::ptr;

/// Find a CoreAudio device by its UID string.
pub fn find_device_by_uid(target_uid: &str) -> Option<AudioObjectID> {
    let device_ids = get_all_devices()?;

    for &device_id in &device_ids {
        if let Some(uid) = get_device_uid(device_id) {
            if uid == target_uid {
                return Some(device_id);
            }
        }
    }

    None
}

/// Enumerate all audio devices on the system.
fn get_all_devices() -> Option<Vec<AudioObjectID>> {
    let property_address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDevices,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };

    let mut data_size: u32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            kAudioObjectSystemObject,
            &property_address,
            0,
            ptr::null(),
            &mut data_size,
        )
    };
    if status != 0 || data_size == 0 {
        return None;
    }

    let device_count = data_size as usize / mem::size_of::<AudioObjectID>();
    let mut devices = vec![0u32; device_count];

    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &property_address,
            0,
            ptr::null(),
            &mut data_size,
            devices.as_mut_ptr().cast(),
        )
    };
    if status != 0 {
        return None;
    }

    Some(devices)
}

/// Get the UID string for a given audio device.
fn get_device_uid(device_id: AudioObjectID) -> Option<String> {
    let property_address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyDeviceUID,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };

    let mut cf_string_ref: CFStringRef = ptr::null();
    let mut data_size = mem::size_of::<CFStringRef>() as u32;

    let status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            &property_address,
            0,
            ptr::null(),
            &mut data_size,
            (&mut cf_string_ref as *mut CFStringRef).cast(),
        )
    };
    if status != 0 || cf_string_ref.is_null() {
        return None;
    }

    // Safety: CoreAudio returns a +1 retained CFString; wrap so it gets released.
    let cf_string: CFString = unsafe { CFString::wrap_under_create_rule(cf_string_ref) };
    Some(cf_string.to_string())
}
