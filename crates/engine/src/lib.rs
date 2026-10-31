//! Claude: `Router` spawns SRT and WebRTC links on the current Tokio runtime, so
//! create network sources and routes from within one.
//!
//! Claude: on macOS the engine taps apps and plays to Core Audio devices. Other
//! Apple platforms have no Core Audio HAL, so there a program brings its own
//! audio I/O through `Router::add_program_input` and `add_program_output`.

mod abl;
mod adts;
mod clock;
mod codec;
mod delivery;
mod flow;
mod host_time;
mod mixer;
mod playout;
mod program_io;
mod ring;
#[cfg(test)]
mod ring_model;
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
mod audio;
#[cfg(target_os = "macos")]
mod bus;
#[cfg(target_os = "macos")]
mod device_manager;
#[cfg(target_os = "macos")]
mod device_output;
#[cfg(target_os = "macos")]
mod tap;

#[cfg(not(target_os = "macos"))]
mod no_hal;
#[cfg(not(target_os = "macos"))]
use no_hal::{audio, bus, device_output, tap};

pub use host_time::HostTime;
pub use program_io::{
    BufferError, Frames, MAX_CALLBACK_FRAMES, MAX_PROGRAM_ROUTES, ProgramInput, ProgramOutput,
};
pub use router::Router;
pub use spec::{Destination, InProcess, Playback, RouteInfo, SourceInfo, SourceSpec};

#[cfg(target_os = "macos")]
pub use activity::{ActivityReader, FpsTracker};
#[cfg(target_os = "macos")]
pub use audio::{find_device_by_uid, list_all_devices};
#[cfg(target_os = "macos")]
pub use device_manager::DeviceManager;
#[cfg(target_os = "macos")]
pub use tap::list_audio_apps;
