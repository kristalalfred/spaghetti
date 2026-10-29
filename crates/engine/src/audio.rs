use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_foundation_sys::string::CFStringRef;
use coreaudio_sys::*;
use spaghetti_common::ipc_protocol::SystemDeviceInfo;

use crate::tap;
use std::mem;
use std::ptr;

/// Find a CoreAudio device by its UID string.
pub fn find_device_by_uid(target_uid: &str) -> Option<AudioObjectID> {
    let device_ids = get_all_devices()?;

    device_ids
        .iter()
        .find(|&&id| get_device_uid(id).as_deref() == Some(target_uid))
        .copied()
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

/// Enumerate all CoreAudio devices on the system with their name and output channel count.
///
/// Devices with no output streams are included with `is_output: false` so the
/// caller can decide whether to filter them.
pub fn list_all_devices() -> Vec<SystemDeviceInfo> {
    let device_ids = match get_all_devices() {
        Some(ids) => ids,
        None => return Vec::new(),
    };

    device_ids
        .into_iter()
        .filter_map(|id| {
            let uid = get_device_uid(id).filter(|uid| !uid.starts_with(tap::AGGREGATE_UID_PREFIX))?;
            let name = get_device_name(id).unwrap_or_else(|| uid.clone());
            let channels = get_output_channel_count(id);
            Some(SystemDeviceInfo {
                uid,
                name,
                channels,
                is_output: channels > 0,
            })
        })
        .collect()
}

/// Get the human-readable name of a device via `kAudioObjectPropertyName`.
fn get_device_name(device_id: AudioObjectID) -> Option<String> {
    let property_address = AudioObjectPropertyAddress {
        mSelector: kAudioObjectPropertyName,
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

/// Sum the output channel count across all output-scope stream buffers.
fn get_output_channel_count(device_id: AudioObjectID) -> u32 {
    let property_address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreamConfiguration,
        mScope: kAudioObjectPropertyScopeOutput,
        mElement: kAudioObjectPropertyElementMain,
    };

    let mut data_size: u32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            device_id,
            &property_address,
            0,
            ptr::null(),
            &mut data_size,
        )
    };
    if status != 0 || data_size < mem::size_of::<AudioBufferList>() as u32 {
        return 0;
    }

    // Allocate a buffer large enough to hold the variable-length AudioBufferList.
    let mut buf = vec![0u8; data_size as usize];
    let status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            &property_address,
            0,
            ptr::null(),
            &mut data_size,
            buf.as_mut_ptr().cast(),
        )
    };
    if status != 0 {
        return 0;
    }

    // Safety: buf is large enough and properly aligned for AudioBufferList.
    let list = unsafe { &*(buf.as_ptr() as *const AudioBufferList) };
    let n = list.mNumberBuffers as usize;
    // The mBuffers field is declared as [AudioBuffer; 1] but is actually n elements.
    let buffers =
        unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), n) };
    buffers.iter().map(|b| b.mNumberChannels).sum()
}

/// Claude: how many frames the device takes from its output time stamp until
/// they sound: its own latency plus its first output stream's.
pub fn output_latency_frames(device_id: AudioObjectID) -> u32 {
    let device = get_u32(
        device_id,
        kAudioDevicePropertyLatency,
        kAudioObjectPropertyScopeOutput,
    );
    let stream = first_output_stream(device_id).and_then(|stream| {
        get_u32(
            stream,
            kAudioStreamPropertyLatency,
            kAudioObjectPropertyScopeGlobal,
        )
    });
    device.unwrap_or(0) + stream.unwrap_or(0)
}

fn get_u32(object: AudioObjectID, selector: u32, scope: u32) -> Option<u32> {
    let property_address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut value: u32 = 0;
    let mut size = mem::size_of::<u32>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &property_address,
            0,
            ptr::null(),
            &mut size,
            (&mut value as *mut u32).cast(),
        )
    };
    (status == 0).then_some(value)
}

fn first_output_stream(device_id: AudioObjectID) -> Option<AudioObjectID> {
    let property_address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreams,
        mScope: kAudioObjectPropertyScopeOutput,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut streams = [0 as AudioObjectID; 16];
    let mut size = mem::size_of_val(&streams) as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            &property_address,
            0,
            ptr::null(),
            &mut size,
            streams.as_mut_ptr().cast(),
        )
    };
    (status == 0 && size >= mem::size_of::<AudioObjectID>() as u32).then_some(streams[0])
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
