use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_foundation_sys::string::CFStringRef;
use coreaudio_sys::*;
use spaghetti_common::ipc_protocol::SystemDeviceInfo;

use crate::{abl, tap};
use std::mem;
use std::ptr;

pub type DeviceId = AudioObjectID;

/// Find a CoreAudio device by its UID string.
pub fn find_device_by_uid(target_uid: &str) -> Option<AudioObjectID> {
    let device_ids = get_all_devices()?;

    device_ids
        .iter()
        .find(|&&id| get_device_uid(id).as_deref() == Some(target_uid))
        .copied()
}

/// Claude: the UID of the input device the system records from by default.
pub fn default_input_device() -> Option<String> {
    let property_address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultInputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut device: AudioObjectID = 0;
    let mut size = mem::size_of::<AudioObjectID>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &property_address,
            0,
            ptr::null(),
            &mut size,
            (&mut device as *mut AudioObjectID).cast(),
        )
    };
    (status == 0 && device != kAudioObjectUnknown)
        .then(|| get_device_uid(device))
        .flatten()
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

/// Claude: a Core Audio device as the system lists it, with the channels
/// its input and output streams carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemDevice {
    pub uid: String,
    pub name: String,
    pub input_channels: u32,
    pub output_channels: u32,
}

/// Claude: the wire form lists a device's output channels only.
impl From<SystemDevice> for SystemDeviceInfo {
    fn from(device: SystemDevice) -> Self {
        Self {
            uid: device.uid,
            name: device.name,
            channels: device.output_channels,
            is_output: device.output_channels > 0,
        }
    }
}

/// Claude: every device the system lists but the private aggregates that
/// expose this engine's taps.
pub fn list_all_devices() -> Vec<SystemDevice> {
    let device_ids = match get_all_devices() {
        Some(ids) => ids,
        None => return Vec::new(),
    };
    let channels =
        |id, scope| stream_configuration(id, scope).map_or(0, |buffers| buffers.iter().sum());

    device_ids
        .into_iter()
        .filter_map(|id| {
            let uid =
                get_device_uid(id).filter(|uid| !uid.starts_with(tap::AGGREGATE_UID_PREFIX))?;
            Some(SystemDevice {
                name: device_name(id).unwrap_or_else(|| uid.clone()),
                uid,
                input_channels: channels(id, kAudioObjectPropertyScopeInput),
                output_channels: channels(id, kAudioObjectPropertyScopeOutput),
            })
        })
        .collect()
}

/// Get the human-readable name of a device via `kAudioObjectPropertyName`.
pub(crate) fn device_name(device_id: AudioObjectID) -> Option<String> {
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

/// Claude: the channels of each buffer the device's streams in `scope` fill,
/// in the order an IOProc is handed them.
pub(crate) fn stream_configuration(device_id: AudioObjectID, scope: u32) -> Option<Vec<u32>> {
    let property_address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreamConfiguration,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut data_size: u32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(device_id, &property_address, 0, ptr::null(), &mut data_size)
    };
    if status != 0 || (data_size as usize) < mem::size_of::<AudioBufferList>() {
        return None;
    }
    let mut words = vec![0u64; (data_size as usize).div_ceil(mem::size_of::<u64>())];
    let status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            &property_address,
            0,
            ptr::null(),
            &mut data_size,
            words.as_mut_ptr().cast(),
        )
    };
    if status != 0 {
        return None;
    }
    let buffers = unsafe { abl::buffers(words.as_ptr().cast()) };
    Some(
        buffers
            .iter()
            .map(|buffer| buffer.mNumberChannels)
            .collect(),
    )
}

/// Claude: the rate the device runs at.
pub fn nominal_rate(device_id: AudioObjectID) -> Option<f64> {
    let property_address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyNominalSampleRate,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut rate: f64 = 0.0;
    let mut size = mem::size_of::<f64>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            &property_address,
            0,
            ptr::null(),
            &mut size,
            (&mut rate as *mut f64).cast(),
        )
    };
    (status == 0 && rate > 0.0).then_some(rate)
}

/// Claude: how many frames the device takes from its output time stamp until
/// they sound: its own latency plus its first output stream's.
pub fn output_latency_frames(device_id: AudioObjectID) -> u32 {
    let device = get_u32(
        device_id,
        kAudioDevicePropertyLatency,
        kAudioObjectPropertyScopeOutput,
    );
    let stream = first_stream(device_id, kAudioObjectPropertyScopeOutput).and_then(|stream| {
        get_u32(
            stream,
            kAudioStreamPropertyLatency,
            kAudioObjectPropertyScopeGlobal,
        )
    });
    device.unwrap_or(0) + stream.unwrap_or(0)
}

pub(crate) fn get_u32(object: AudioObjectID, selector: u32, scope: u32) -> Option<u32> {
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

pub(crate) fn first_stream(device_id: AudioObjectID, scope: u32) -> Option<AudioObjectID> {
    let property_address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreams,
        mScope: scope,
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

/// Claude: the format a stream hands its clients.
pub(crate) fn virtual_format(stream: AudioObjectID) -> Option<AudioStreamBasicDescription> {
    let property_address = AudioObjectPropertyAddress {
        mSelector: kAudioStreamPropertyVirtualFormat,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut format: AudioStreamBasicDescription = unsafe { mem::zeroed() };
    let mut size = mem::size_of::<AudioStreamBasicDescription>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            stream,
            &property_address,
            0,
            ptr::null(),
            &mut size,
            (&mut format as *mut AudioStreamBasicDescription).cast(),
        )
    };
    (status == 0).then_some(format)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_listing_keeps_its_shape_and_lists_output_channels() {
        for (input_channels, output_channels, is_output) in
            [(2, 0, false), (1, 2, true), (0, 8, true)]
        {
            let wire = SystemDeviceInfo::from(SystemDevice {
                uid: "d".into(),
                name: "D".into(),
                input_channels,
                output_channels,
            });
            assert_eq!(
                serde_json::to_string(&wire).unwrap(),
                format!(
                    r#"{{"uid":"d","name":"D","channels":{output_channels},"is_output":{is_output}}}"#
                )
            );
        }
    }
}
