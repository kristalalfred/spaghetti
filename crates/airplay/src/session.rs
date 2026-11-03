use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use tracing::debug;

use crate::airplay2;
use crate::events::EventChannel;
use crate::hap::Counted;
use crate::packet::{self, AudioHeader, Framing, MAX_AUDIO_PACKET_LEN};
use crate::rtsp::{Connection, Request, Response};
use crate::{FRAMES_PER_PACKET, Protocol};

/// Claude: what a realtime stream offers the receiver in its handshake.
pub(crate) struct Offer {
    pub(crate) control_port: u16,
    pub(crate) timing_port: u16,
    pub(crate) first_seq: u16,
    pub(crate) first_rtp: u32,
}

/// Claude: a receiver that accepted a stream: its RTSP connection and what
/// the protocol keeps for the rest of the session.
pub(crate) struct Session {
    pub(crate) connection: Connection,
    pub(crate) uri: String,
    pub(crate) transport: Transport,
}

pub(crate) enum Transport {
    AirPlay1,
    /// Claude: the receiver's event connection, answered on a thread of its
    /// own when it could be opened, and the stream its TEARDOWN names.
    AirPlay2 {
        _events: Option<EventChannel>,
        stream: StreamRef,
    },
}

/// Claude: an AirPlay 2 stream as a TEARDOWN names it: its type, and its ID
/// when the receiver gave one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamRef {
    pub(crate) kind: u64,
    pub(crate) id: Option<u64>,
}

/// Claude: where a receiver takes a realtime stream's audio and control
/// packets, how it takes them, and the latency it adds to what sync packets
/// say.
pub(crate) struct RealtimeAudio {
    pub(crate) audio: SocketAddr,
    pub(crate) control: SocketAddr,
    pub(crate) latency: AudioLatency,
    pub(crate) packets: Packets,
}

pub(crate) enum Packets {
    AirPlay1,
    /// Claude: sealed with the key the stream's SETUP gave the receiver.
    AirPlay2(Counted),
}

/// Claude: how a receiver takes a packet it asked for again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resend {
    /// Claude: wrapped in a retransmission, over the control channel.
    Wrapped,
    /// Claude: as first sent, on the audio channel, as OwnTone resends to
    /// AirPlay 2 receivers. A Harman soundbar on Apple's AirPlay SDK 2.0.4
    /// asked once for each packet it lost and concealed it when the packet
    /// came back wrapped over control, but played it when it came this way.
    Original,
}

/// Claude: the request that keeps a session alive, and how often it goes.
/// An AirPlay 2 receiver's answer to `/feedback` counts whatever its status,
/// as OwnTone and pyatv take it.
pub(crate) struct KeepAlive {
    pub(crate) method: &'static str,
    pub(crate) uri: &'static str,
    pub(crate) every: Duration,
    pub(crate) needs_ok: bool,
}

impl Packets {
    pub(crate) fn resend(&self) -> Resend {
        match self {
            Packets::AirPlay1 => Resend::Wrapped,
            Packets::AirPlay2(_) => Resend::Original,
        }
    }

    /// Claude: writes the packet the receiver gets for these frames into
    /// `out` and returns its length.
    pub(crate) fn write(
        &mut self,
        header: &AudioHeader,
        left: &[f32; FRAMES_PER_PACKET],
        right: &[f32; FRAMES_PER_PACKET],
        out: &mut [u8; MAX_AUDIO_PACKET_LEN],
    ) -> usize {
        match self {
            Packets::AirPlay1 => {
                packet::write_audio_packet(out, header, left, right, Framing::Counted)
            }
            Packets::AirPlay2(audio_key) => {
                let len = packet::write_audio_packet(out, header, left, right, Framing::Full);
                packet::seal_audio(audio_key, out, len)
            }
        }
    }
}

impl Session {
    pub(crate) fn protocol(&self) -> Protocol {
        match self.transport {
            Transport::AirPlay1 => Protocol::AirPlay1,
            Transport::AirPlay2 { .. } => Protocol::AirPlay2,
        }
    }

    pub(crate) fn keepalive(&self) -> KeepAlive {
        match self.transport {
            Transport::AirPlay1 => KeepAlive {
                method: "OPTIONS",
                uri: "*",
                every: Duration::from_secs(25),
                needs_ok: true,
            },
            Transport::AirPlay2 { .. } => KeepAlive {
                method: "POST",
                uri: "/feedback",
                every: airplay2::FEEDBACK_INTERVAL,
                needs_ok: false,
            },
        }
    }

    /// Claude: tears the session down within `timeout`.
    pub(crate) fn teardown(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let requests = match &self.transport {
            Transport::AirPlay1 => vec![(None, "TEARDOWN")],
            Transport::AirPlay2 { stream, .. } => vec![
                (
                    Some(airplay2::stream_teardown(*stream)),
                    "TEARDOWN of the stream",
                ),
                (
                    Some(airplay2::session_teardown()),
                    "TEARDOWN of the session",
                ),
            ],
        };
        for (body, name) in requests {
            let request = Request {
                method: "TEARDOWN",
                uri: &self.uri,
                headers: vec![],
                body: body.as_deref().map(|body| (crate::rtsp::PLIST, body)),
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.connection.exchange(&request, remaining, &|| false) {
                Ok(response) => debug!(status = response.status, "{name} answered"),
                Err(reason) => {
                    debug!(%reason, "{name} unanswered");
                    break;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioLatency {
    Reported(u32),
    Assumed(u32),
}

impl AudioLatency {
    pub(crate) fn frames(self) -> u32 {
        match self {
            AudioLatency::Reported(frames) | AudioLatency::Assumed(frames) => frames,
        }
    }
}

/// Claude: what a receiver whose RECORD answer has no `Audio-Latency` is taken
/// to add: 11025 frames (0.25 s), what an AirPort Express reports and what
/// OwnTone allows for every RAOP receiver whatever it reports. An HK Citation
/// MultiBeam 1100 that reports none played about 0.23 s late when taken to add
/// nothing. An AirPlay 2 stream asks for it as its `latencyMin`.
pub(crate) const ASSUMED_AUDIO_LATENCY: u32 = 11_025;

pub(crate) fn audio_latency(record: &Response) -> AudioLatency {
    match record.header("Audio-Latency").and_then(|l| l.parse().ok()) {
        Some(frames) => AudioLatency::Reported(frames),
        None => AudioLatency::Assumed(ASSUMED_AUDIO_LATENCY),
    }
}

pub(crate) fn uri(local: IpAddr, session_id: u32) -> String {
    match local {
        IpAddr::V4(ip) => format!("rtsp://{ip}/{session_id}"),
        IpAddr::V6(ip) => format!("rtsp://[{ip}]/{session_id}"),
    }
}

pub(crate) fn refusal(method: &str, response: &Response) -> String {
    let status = format!("{} {}", response.status, response.reason);
    match response.status {
        401 => format!("it requires a password ({method} answered {status})"),
        453 => format!("it is busy with another sender ({method} answered {status})"),
        470 => format!("it requires pairing with a PIN ({method} answered {status})"),
        _ => format!("it refused {method} with {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_answer_without_a_readable_audio_latency_is_taken_to_add_a_quarter_second() {
        let record = |headers: &[(&str, &str)]| Response {
            status: 200,
            reason: "OK".into(),
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            body: Vec::new(),
        };
        assert_eq!(
            audio_latency(&record(&[("Audio-Latency", "4410")])),
            AudioLatency::Reported(4410)
        );
        assert_eq!(
            audio_latency(&record(&[("audio-latency", "0")])),
            AudioLatency::Reported(0)
        );
        assert_eq!(audio_latency(&record(&[])), AudioLatency::Assumed(11_025));
        assert_eq!(
            audio_latency(&record(&[("Audio-Latency", "a lot")])),
            AudioLatency::Assumed(11_025)
        );
    }
}
