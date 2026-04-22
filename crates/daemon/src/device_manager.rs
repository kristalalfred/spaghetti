use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFMutableDictionary;
use core_foundation::string::CFString;
use coreaudio_sys::{AudioObjectID, AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice};
use spaghetti_common::ipc_protocol::DeviceInfo;

const SPAGHETTI_DEVICE_UID: &str = "SpaghettiDevice_UID";

struct ManagedDevice {
    aggregate_id: AudioObjectID,
    uid: String,
    name: String,
    channels: u32,
}

pub struct DeviceManager {
    devices: HashMap<String, ManagedDevice>,
}

impl DeviceManager {
    pub fn new() -> Self {
        Self {
            devices: HashMap::new(),
        }
    }

    /// Create a new aggregate device wrapping the static Spaghetti device.
    pub fn create_device(&mut self, name: &str, channels: u32) -> Result<String, String> {
        let uid = format!("spaghetti_virtual_{}", uuid_simple());

        // Build sub-device entry referencing the static Spaghetti device
        let mut sub_device = CFMutableDictionary::new();
        sub_device.set(
            CFString::new("uid").as_CFType(),
            CFString::new(SPAGHETTI_DEVICE_UID).as_CFType(),
        );

        let sub_devices = vec![sub_device.as_CFType()];
        let sub_device_array = core_foundation::array::CFArray::from_CFTypes(&sub_devices);

        // Build aggregate device description
        let mut desc = CFMutableDictionary::new();
        desc.set(
            CFString::new("uid").as_CFType(),
            CFString::new(&uid).as_CFType(),
        );
        desc.set(
            CFString::new("name").as_CFType(),
            CFString::new(name).as_CFType(),
        );
        desc.set(
            CFString::new("subdevices").as_CFType(),
            sub_device_array.as_CFType(),
        );
        desc.set(
            CFString::new("private").as_CFType(),
            CFBoolean::false_value().as_CFType(),
        );

        let mut aggregate_id: AudioObjectID = 0;
        // core-foundation and coreaudio-sys define separate __CFDictionary types;
        // cast through a raw pointer to bridge them.
        let desc_ptr = desc.as_concrete_TypeRef().cast::<coreaudio_sys::__CFDictionary>();
        let status = unsafe {
            AudioHardwareCreateAggregateDevice(desc_ptr, &mut aggregate_id)
        };

        if status != 0 {
            return Err(format!("AudioHardwareCreateAggregateDevice failed: {status}"));
        }

        tracing::info!(uid = %uid, name = %name, aggregate_id, "created aggregate device");

        self.devices.insert(uid.clone(), ManagedDevice {
            aggregate_id,
            uid: uid.clone(),
            name: name.to_string(),
            channels,
        });

        Ok(uid)
    }

    /// Destroy a previously created aggregate device.
    pub fn destroy_device(&mut self, uid: &str) -> Result<(), String> {
        let device = self
            .devices
            .remove(uid)
            .ok_or_else(|| format!("device '{uid}' not found"))?;

        let status = unsafe { AudioHardwareDestroyAggregateDevice(device.aggregate_id) };
        if status != 0 {
            return Err(format!("AudioHardwareDestroyAggregateDevice failed: {status}"));
        }

        tracing::info!(uid = %uid, "destroyed aggregate device");
        Ok(())
    }

    /// List all managed virtual devices.
    pub fn list_devices(&self) -> Vec<DeviceInfo> {
        self.devices
            .values()
            .map(|d| DeviceInfo {
                uid: d.uid.clone(),
                name: d.name.clone(),
                channels: d.channels,
            })
            .collect()
    }

}

impl Drop for DeviceManager {
    fn drop(&mut self) {
        for (uid, device) in self.devices.drain() {
            let status =
                unsafe { AudioHardwareDestroyAggregateDevice(device.aggregate_id) };
            if status != 0 {
                tracing::warn!(uid = %uid, status, "failed to destroy aggregate device on cleanup");
            } else {
                tracing::info!(uid = %uid, "cleaned up aggregate device");
            }
        }
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn uuid_simple() -> String {
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("{:x}{:04x}", t.as_millis(), seq)
}
