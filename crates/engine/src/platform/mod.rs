//! Claude: the audio hardware the router taps apps from, captures inputs
//! from and plays devices on. Each backend names the same modules: `audio`
//! (devices), `tap`, `input`, `bus` and `device_output`. A tap routed to a
//! device without a delay mixes straight into the device through its `bus`
//! where the backend has one (`bus::DIRECT`), and through a ring otherwise.

#[cfg(target_os = "macos")]
mod coreaudio;
#[cfg(target_os = "macos")]
pub use coreaudio::{audio, bus, device_output, input, tap};

#[cfg(windows)]
mod wasapi;
#[cfg(windows)]
pub use wasapi::{audio, device_output, input, tap};

#[cfg(not(any(target_os = "macos", windows)))]
mod none;
#[cfg(not(any(target_os = "macos", windows)))]
pub use none::{audio, device_output, input, tap};
#[cfg(not(target_os = "macos"))]
#[path = "no_bus.rs"]
pub mod bus;

use spaghetti_common::ipc_protocol::SystemDeviceInfo;

/// Claude: a device as the system lists it, with the channels its input and
/// output streams carry.
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
