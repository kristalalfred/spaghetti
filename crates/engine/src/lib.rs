//! Claude: `Router` spawns SRT and WebRTC links on the current Tokio runtime, so
//! create network sources and routes from within one.

mod abl;
mod activity;
mod adts;
mod audio;
mod bus;
mod clock;
mod codec;
mod delivery;
mod device_manager;
mod device_output;
#[cfg(test)]
mod fixture;
mod flow;
mod mixer;
mod playout;
mod ring;
mod router;
mod rt_slots;
mod sinks;
mod srt;
mod srt_receiver;
mod srt_sender;
mod stun;
mod tap;
mod ts;
mod turn;
mod webrtc;
mod webrtc_receiver;
mod webrtc_sender;
#[cfg(test)]
mod webrtc_tests;

pub use activity::{ActivityReader, FpsTracker};
pub use audio::{find_device_by_uid, list_all_devices};
pub use device_manager::DeviceManager;
pub use router::Router;
pub use tap::list_audio_apps;
