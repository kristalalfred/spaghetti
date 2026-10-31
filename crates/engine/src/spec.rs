//! Claude: what the engine's sources read and its routes play to. The IPC
//! protocol carries the sources and destinations another process can ask
//! for; a program embedding the engine also names the inputs and outputs it
//! registered, which exist only in its own process.

use spaghetti_common::ipc_protocol::{self as wire, FlowStatus, SrtSocket, TapMode, WebRtcPeer};

/// Claude: what a source reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSpec {
    /// Claude: the audio of these processes, mixed to stereo.
    Tap { pids: Vec<u32>, mode: TapMode },
    /// Claude: AAC in MPEG-TS received over SRT, decoded to 48 kHz stereo.
    Srt(SrtSocket),
    /// Claude: Opus received from one WebRTC peer, decoded to 48 kHz stereo.
    WebRtc(WebRtcPeer),
    /// Claude: the program input registered under this name.
    Program { input: String },
}

/// Claude: where a route plays its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// Claude: a Core Audio output device, by UID. With `delay_ms`, each frame
    /// sounds that long after its source captured it.
    Device { uid: String, delay_ms: Option<u32> },
    /// Claude: AAC in MPEG-TS sent over SRT.
    Srt(SrtSocket),
    /// Claude: Opus sent to one WebRTC peer.
    WebRtc(WebRtcPeer),
    /// Claude: the program output registered under this name, mixed with the
    /// other routes to it. With `delay_ms`, each frame is heard that long
    /// after its source captured it.
    Program {
        output: String,
        delay_ms: Option<u32>,
    },
}

/// Claude: whether a route's output is playing it, as of the output's latest
/// render. An output that runs renders again within 100 ms plus the time 4096
/// frames take at its rate, 185 ms at 48 kHz; past that it counts as stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playback {
    /// Claude: the latest render played the route's source, at whatever volume.
    Playing,
    /// Claude: the latest render played the route silence: the route waits for
    /// enough audio or for its schedule, or the output refused the render.
    Silent,
    /// Claude: the output has not rendered within 100 ms plus the time 4096
    /// frames take at its rate.
    Stopped,
}

#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub source_id: u32,
    pub spec: SourceSpec,
    pub tag: Option<String>,
    /// Claude: the tapped app's name, the SRT endpoint, the WebRTC link, or
    /// the program input's name.
    pub label: String,
    pub status: FlowStatus,
    /// Claude: a WebRTC peer's offer or answer, once it has one.
    pub local_description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RouteInfo {
    pub route_id: u32,
    pub source_id: u32,
    pub destination: Destination,
    pub volume: f32,
    pub status: FlowStatus,
    /// Claude: for a route to a device or a program output; a network route
    /// has no output of its own to play it.
    pub playback: Option<Playback>,
    /// Claude: a WebRTC peer's offer or answer, once it has one.
    pub local_description: Option<String>,
}

impl From<wire::SourceSpec> for SourceSpec {
    fn from(spec: wire::SourceSpec) -> Self {
        match spec {
            wire::SourceSpec::Tap { pids, mode } => Self::Tap { pids, mode },
            wire::SourceSpec::Srt(socket) => Self::Srt(socket),
            wire::SourceSpec::WebRtc(peer) => Self::WebRtc(peer),
        }
    }
}

impl From<wire::Destination> for Destination {
    fn from(destination: wire::Destination) -> Self {
        match destination {
            wire::Destination::Device { uid, delay_ms } => Self::Device { uid, delay_ms },
            wire::Destination::Srt(socket) => Self::Srt(socket),
            wire::Destination::WebRtc(peer) => Self::WebRtc(peer),
        }
    }
}

/// Claude: a source or destination that only means something in the
/// process that registered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InProcess(pub String);

impl std::fmt::Display for InProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} exists only in the process that registered it",
            self.0
        )
    }
}

impl std::error::Error for InProcess {}

impl TryFrom<SourceSpec> for wire::SourceSpec {
    type Error = InProcess;

    fn try_from(spec: SourceSpec) -> Result<Self, InProcess> {
        match spec {
            SourceSpec::Tap { pids, mode } => Ok(Self::Tap { pids, mode }),
            SourceSpec::Srt(socket) => Ok(Self::Srt(socket)),
            SourceSpec::WebRtc(peer) => Ok(Self::WebRtc(peer)),
            SourceSpec::Program { input } => Err(InProcess(format!("program input '{input}'"))),
        }
    }
}

impl TryFrom<Destination> for wire::Destination {
    type Error = InProcess;

    fn try_from(destination: Destination) -> Result<Self, InProcess> {
        match destination {
            Destination::Device { uid, delay_ms } => Ok(Self::Device { uid, delay_ms }),
            Destination::Srt(socket) => Ok(Self::Srt(socket)),
            Destination::WebRtc(peer) => Ok(Self::WebRtc(peer)),
            Destination::Program { output, .. } => {
                Err(InProcess(format!("program output '{output}'")))
            }
        }
    }
}

impl TryFrom<SourceInfo> for wire::SourceInfo {
    type Error = InProcess;

    fn try_from(info: SourceInfo) -> Result<Self, InProcess> {
        Ok(Self {
            source_id: info.source_id,
            spec: info.spec.try_into()?,
            tag: info.tag,
            label: info.label,
            status: info.status,
            local_description: info.local_description,
        })
    }
}

impl TryFrom<RouteInfo> for wire::RouteInfo {
    type Error = InProcess;

    fn try_from(info: RouteInfo) -> Result<Self, InProcess> {
        Ok(Self {
            route_id: info.route_id,
            source_id: info.source_id,
            destination: info.destination.try_into()?,
            volume: info.volume,
            status: info.status,
            local_description: info.local_description,
        })
    }
}

#[cfg(test)]
mod tests {
    use spaghetti_common::ipc_protocol::{FlowState, PeerRole, SrtMode};

    use super::*;

    fn srt() -> SrtSocket {
        SrtSocket {
            mode: SrtMode::Listen { port: 9000 },
            latency_ms: 120,
            passphrase: None,
            key_size: None,
        }
    }

    #[test]
    fn every_wire_source_and_destination_converts_there_and_back() {
        let peer = WebRtcPeer {
            link: "a".into(),
            generation: 1,
            role: PeerRole::Offer,
            stun: Vec::new(),
            turn: Vec::new(),
        };
        let sources = [
            wire::SourceSpec::Tap {
                pids: vec![1, 2],
                mode: TapMode::Mirror,
            },
            wire::SourceSpec::Srt(srt()),
            wire::SourceSpec::WebRtc(peer.clone()),
        ];
        for spec in sources {
            let back: wire::SourceSpec = SourceSpec::from(spec.clone()).try_into().unwrap();
            assert_eq!(back, spec);
        }
        let destinations = [
            wire::Destination::Device {
                uid: "d".into(),
                delay_ms: Some(300),
            },
            wire::Destination::Device {
                uid: "d".into(),
                delay_ms: None,
            },
            wire::Destination::Srt(srt()),
            wire::Destination::WebRtc(peer),
        ];
        for destination in destinations {
            let back: wire::Destination =
                Destination::from(destination.clone()).try_into().unwrap();
            assert_eq!(back, destination);
        }
    }

    #[test]
    fn program_endpoints_have_no_wire_form() {
        let route = RouteInfo {
            route_id: 1,
            source_id: 1,
            destination: Destination::Program {
                output: "speaker".into(),
                delay_ms: None,
            },
            volume: 1.0,
            status: FlowStatus {
                state: FlowState::Idle,
                bytes: 0,
                peer: None,
                relayed: false,
                packets_lost: 0,
                packets_retransmitted: 0,
            },
            playback: Some(Playback::Stopped),
            local_description: None,
        };
        let refused = wire::RouteInfo::try_from(route).unwrap_err();
        assert_eq!(refused, InProcess("program output 'speaker'".into()));
        let refused = wire::SourceSpec::try_from(SourceSpec::Program {
            input: "mic".into(),
        });
        assert!(refused.is_err());
    }
}
