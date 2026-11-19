//! Claude: the audio hardware the router taps apps from, captures inputs
//! from and plays devices on. Each backend names the same modules: `audio`
//! (devices), `tap`, `input`, `bus` and `device_output`.

#[cfg(target_os = "macos")]
mod coreaudio;
#[cfg(target_os = "macos")]
pub use coreaudio::{audio, bus, device_output, input, tap};

#[cfg(not(target_os = "macos"))]
mod none;
#[cfg(not(target_os = "macos"))]
pub use none::{audio, bus, device_output, input, tap};
