use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, EDataFlow, IMMDevice, IMMEndpoint, eAll, eCapture, eConsole,
};
use windows::Win32::System::Com::STGM_READ;
use windows::core::Interface;

use super::com;
use crate::platform::SystemDevice;

/// Claude: an endpoint's ID, which is also its UID.
pub type DeviceId = String;

pub fn find_device_by_uid(uid: &str) -> Option<DeviceId> {
    com::device(uid).ok().map(|_| uid.to_string())
}

pub fn default_input_device() -> Option<String> {
    default_device(eCapture)
}

fn default_device(flow: EDataFlow) -> Option<String> {
    let device = unsafe {
        com::enumerator()
            .ok()?
            .GetDefaultAudioEndpoint(flow, eConsole)
    }
    .ok()?;
    com::id(&device)
}

/// Claude: every active endpoint. An endpoint carries one direction, so a
/// device with inputs and outputs is listed as two.
pub fn list_all_devices() -> Vec<SystemDevice> {
    let Ok(enumerator) = com::enumerator() else {
        return Vec::new();
    };
    let Ok(collection) = (unsafe { enumerator.EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE) })
    else {
        return Vec::new();
    };
    let count = unsafe { collection.GetCount() }.unwrap_or(0);
    (0..count)
        .filter_map(|i| {
            let device = unsafe { collection.Item(i) }.ok()?;
            let uid = com::id(&device)?;
            let channels = com::activate(&device)
                .and_then(|client| com::mix_format(&client))
                .map_or(0, |format| u32::from(format.channels));
            let input = flow(&device)? == eCapture;
            Some(SystemDevice {
                name: name(&device).unwrap_or_else(|| uid.clone()),
                uid,
                input_channels: if input { channels } else { 0 },
                output_channels: if input { 0 } else { channels },
            })
        })
        .collect()
}

pub(crate) fn name(device: &IMMDevice) -> Option<String> {
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }.ok()?;
    let value = unsafe { store.GetValue(&PKEY_Device_FriendlyName) }.ok()?;
    Some(value.to_string()).filter(|name| !name.is_empty())
}

pub(crate) fn flow(device: &IMMDevice) -> Option<EDataFlow> {
    unsafe { device.cast::<IMMEndpoint>().ok()?.GetDataFlow() }.ok()
}
