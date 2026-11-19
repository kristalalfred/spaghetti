//! Claude: `Router` spawns SRT and WebRTC links on the current Tokio runtime, so
//! create network sources and routes from within one.
//!
//! Claude: on macOS the engine taps apps and plays to Core Audio devices. Other
//! Apple platforms have no Core Audio HAL, so there a program brings its own
//! audio I/O through `Router::add_program_input` and `add_program_output`.

#[cfg(target_vendor = "apple")]
mod abl;
mod adts;
mod airplay_sender;
mod calibration;
mod clock;
mod codec;
mod delivery;
#[cfg(test)]
mod device_lookup;
mod flow;
mod host_time;
mod leases;
mod mixer;
mod playout;
mod program_io;
mod reservations;
mod ring;
#[cfg(test)]
mod ring_model;
mod route_output;
mod router;
mod rt_slots;
mod sinks;
mod spec;
mod srt;
mod srt_receiver;
mod srt_sender;
mod stream_clock;
mod stun;
mod ts;
mod turn;
mod webrtc;
mod webrtc_receiver;
mod webrtc_sender;
#[cfg(test)]
mod webrtc_tests;

#[cfg(target_os = "macos")]
mod activity;
#[cfg(target_os = "macos")]
mod device_manager;
mod platform;

use platform::{audio, bus, device_output, input, tap};

pub use calibration::{
    Calibrated, Calibrating, Calibration, CalibrationFailure, CalibrationState, Movement,
    ProbeFailure, Recording, RecordingFailure, Residual, Role,
};
pub use host_time::HostTime;
pub use platform::SystemDevice;
pub use program_io::{
    BufferError, Frames, MAX_CALLBACK_FRAMES, MAX_PROGRAM_RATE, MAX_PROGRAM_ROUTES,
    MIN_PROGRAM_RATE, ProgramInput, ProgramOutput,
};
pub use router::{Router, list_airplay_receivers};
pub use spec::{Destination, InProcess, Playback, RouteInfo, SourceInfo, SourceSpec};

#[cfg(target_os = "macos")]
pub use activity::{ActivityReader, FpsTracker};
#[cfg(any(target_os = "macos", windows))]
pub use audio::{default_input_device, find_device_by_uid, list_all_devices};
#[cfg(target_os = "macos")]
pub use device_manager::DeviceManager;
#[cfg(any(target_os = "macos", windows))]
pub use tap::list_audio_apps;
