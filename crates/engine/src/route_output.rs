//! Claude: what plays a ring route: a device, an AirPlay receiver or a
//! program output, or a sender over SRT or WebRTC. The router resolves a
//! destination to a `Target` first, so an output opens the same way whoever
//! opens it.

use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use spaghetti_common::ipc_protocol::{FlowStatus, SrtSocket, WebRtcPeer};

use crate::airplay_sender::AirPlayOutput;
use crate::audio::DeviceId;
use crate::device_output::DeviceOutput;
use crate::mixer::{Cycles, IDLE};
use crate::playout::Playout;
use crate::program_io::{OutputShared, ProgramRoute};
use crate::ring::Ring;
use crate::router::airplay_browser;
use crate::sinks::SourceTiming;
use crate::spec::Playback;
use crate::srt_sender::SrtOutput;
use crate::webrtc::PeerLink;
use crate::webrtc_sender::WebRtcOutput;

/// Claude: a destination as the router resolved it: the device found, the
/// AirPlay schedule checked and the receiver free, the program output looked up.
pub(crate) enum Target {
    Device {
        device: DeviceId,
        delay: Option<f64>,
        latency_offset_ms: Option<i32>,
    },
    AirPlay {
        receiver: airplay::ReceiverId,
        playout: Box<Playout>,
    },
    Program {
        output: Arc<OutputShared>,
        delay: Option<f64>,
        latency_offset_ms: Option<i32>,
    },
    Srt(SrtSocket),
    WebRtc(WebRtcPeer),
}

pub(crate) enum Output {
    Device(DeviceOutput),
    AirPlay(AirPlayOutput),
    Srt(SrtOutput),
    WebRtc(WebRtcOutput),
    Program(ProgramRoute),
}

impl Output {
    /// Claude: opens the output `target` names for a source of `timing`, and
    /// the ring the source feeds it through. It plays once started.
    pub(crate) fn open(
        target: Target,
        timing: SourceTiming,
        volume: Arc<AtomicU32>,
    ) -> Result<(Self, Arc<Ring>), String> {
        Ok(match target {
            Target::Device {
                device,
                delay,
                latency_offset_ms,
            } => {
                let (output, ring) = unsafe {
                    DeviceOutput::open(device, timing, volume, delay, latency_offset_ms)
                }?;
                (Self::Device(output), ring)
            }
            Target::AirPlay { receiver, playout } => {
                let (output, ring) =
                    AirPlayOutput::open(airplay_browser(), receiver, *playout, volume);
                (Self::AirPlay(output), ring)
            }
            Target::Program {
                output,
                delay,
                latency_offset_ms,
            } => {
                let (output, ring) =
                    ProgramRoute::open(&output, timing, volume, delay, latency_offset_ms)?;
                (Self::Program(output), ring)
            }
            Target::Srt(socket) => {
                let ring = Arc::new(Ring::new(timing.sample_rate));
                let output = SrtOutput::open(&socket, timing, Arc::clone(&ring), volume)?;
                (Self::Srt(output), ring)
            }
            Target::WebRtc(peer) => {
                let ring = Arc::new(Ring::new(timing.sample_rate));
                let output = WebRtcOutput::open(&peer, timing, Arc::clone(&ring), volume)?;
                (Self::WebRtc(output), ring)
            }
        })
    }

    pub(crate) fn start(&self) -> Result<(), String> {
        match self {
            Self::Device(device) => device.start(),
            Self::AirPlay(_) | Self::Srt(_) | Self::WebRtc(_) | Self::Program(_) => Ok(()),
        }
    }

    pub(crate) fn status(&self) -> FlowStatus {
        match self {
            Self::Device(device) => device.status(),
            Self::AirPlay(airplay) => airplay.status(),
            Self::Srt(srt) => srt.status(),
            Self::WebRtc(webrtc) => webrtc.status(),
            Self::Program(program) => program.status(),
        }
    }

    pub(crate) fn log_health(&mut self, route_id: u32, ring: &Ring) {
        match self {
            Self::Device(device) => device.log_health(route_id, ring),
            Self::AirPlay(airplay) => airplay.log_health(route_id, ring),
            Self::Srt(srt) => srt.log_health(route_id, ring),
            Self::WebRtc(webrtc) => webrtc.log_health(route_id, ring),
            Self::Program(program) => program.log_health(route_id, ring),
        }
    }

    pub(crate) fn peer(&self) -> Option<&PeerLink> {
        match self {
            Self::WebRtc(webrtc) => Some(webrtc.peer()),
            _ => None,
        }
    }

    pub(crate) fn playing_cycles(&self) -> Option<&Cycles> {
        match self {
            Self::Device(device) => Some(device.cycles()).filter(|cycles| cycles.running(IDLE)),
            Self::AirPlay(airplay) => airplay.is_playing().then(|| airplay.cycles()),
            Self::Program(program) => Some(program.cycles()).filter(|cycles| cycles.running(IDLE)),
            Self::Srt(_) | Self::WebRtc(_) => None,
        }
    }

    pub(crate) fn playback(&self) -> Option<Playback> {
        match self {
            Self::Device(device) => Some(device.playback()),
            Self::Program(program) => Some(program.playback()),
            Self::AirPlay(_) => None,
            Self::Srt(_) | Self::WebRtc(_) => None,
        }
    }
}
