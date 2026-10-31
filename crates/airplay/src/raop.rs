use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tracing::{debug, info};

use crate::receiver::Txt;
use crate::rtsp::{Connection, Request, Response};
use crate::{FRAMES_PER_PACKET, SAMPLE_RATE};

/// Claude: what a stream offers the receiver in its handshake.
pub(crate) struct Offer {
    pub(crate) control_port: u16,
    pub(crate) timing_port: u16,
    pub(crate) first_seq: u16,
    pub(crate) first_rtp: u32,
}

/// Claude: a receiver that accepted a stream: where to send audio and control
/// packets, and the latency it adds to what sync packets say.
pub(crate) struct Session {
    pub(crate) connection: Connection,
    pub(crate) uri: String,
    pub(crate) audio: SocketAddr,
    pub(crate) control: SocketAddr,
    pub(crate) audio_latency: AudioLatency,
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
/// nothing.
const ASSUMED_AUDIO_LATENCY: u32 = 11_025;

enum Failure {
    AnnounceRefused(Response),
    Other(String),
}

/// Claude: runs the handshake on `connection`. A receiver that refuses the
/// ANNOUNCE and advertises MFi authentication (`et` lists 4) gets the handshake
/// again on a new connection, with `POST /auth-setup` before the ANNOUNCE.
pub(crate) fn negotiate(
    connection: Connection,
    txt: &Txt,
    offer: &Offer,
    timeout: Duration,
    abort: &dyn Fn() -> bool,
) -> Result<Session, String> {
    let peer = connection.peer_addr();
    let common = connection.common_headers().to_vec();
    match attempt(connection, offer, timeout, abort, false) {
        Ok(session) => Ok(session),
        Err(Failure::AnnounceRefused(response)) if retry_with_auth_setup(response.status, txt) => {
            info!(%peer, status = response.status, "receiver refused ANNOUNCE; retrying with auth-setup");
            let connection = Connection::open(&[peer], timeout, common, abort)
                .map_err(|error| error.to_string())?;
            attempt(connection, offer, timeout, abort, true).map_err(|failure| match failure {
                Failure::AnnounceRefused(response) => refusal("ANNOUNCE", &response),
                Failure::Other(reason) => reason,
            })
        }
        Err(Failure::AnnounceRefused(response)) => Err(refusal("ANNOUNCE", &response)),
        Err(Failure::Other(reason)) => Err(reason),
    }
}

/// Claude: OwnTone and libraop send auth-setup to every receiver whose `et`
/// lists 4, but pyatv found receivers advertising it (a Yamaha RX-V773) that
/// play nothing once auth-setup is started and not finished. Receivers that
/// need it refuse the ANNOUNCE without it: 406 from an AirPort Express (pyatv),
/// 403 from AirPlay 2 speakers such as the Sonos Beam (OwnTone).
fn retry_with_auth_setup(announce_status: u16, txt: &Txt) -> bool {
    matches!(announce_status, 403 | 406) && txt.list("et").any(|e| e == "4")
}

fn attempt(
    mut connection: Connection,
    offer: &Offer,
    timeout: Duration,
    abort: &dyn Fn() -> bool,
    auth_setup: bool,
) -> Result<Session, Failure> {
    let local = connection.local_addr().ip();
    let peer = connection.peer_addr();
    let session_id = getrandom::u32().map_err(|e| Failure::Other(e.to_string()))?;
    let uri = match local {
        IpAddr::V4(ip) => format!("rtsp://{ip}/{session_id}"),
        IpAddr::V6(ip) => format!("rtsp://[{ip}]/{session_id}"),
    };
    let mut exchange = |request: Request| {
        let response = connection
            .exchange(&request, timeout, abort)
            .map_err(Failure::Other)?;
        Ok::<_, Failure>(response)
    };
    let expect_ok = |method: &str, response: Response| {
        if response.status == 200 {
            Ok(response)
        } else {
            Err(Failure::Other(refusal(method, &response)))
        }
    };

    let response = exchange(Request {
        method: "OPTIONS",
        uri: "*",
        headers: vec![],
        body: None,
    })?;
    expect_ok("OPTIONS", response)?;

    if auth_setup {
        let mut body = [0u8; 33];
        body[0] = AUTH_SETUP_UNENCRYPTED;
        getrandom::fill(&mut body[1..]).map_err(|e| Failure::Other(e.to_string()))?;
        let response = exchange(Request {
            method: "POST",
            uri: "/auth-setup",
            headers: vec![],
            body: Some(("application/octet-stream", &body)),
        })?;
        debug!(status = response.status, "auth-setup answered");
    }

    let sdp = sdp(session_id, local, peer.ip());
    let response = exchange(Request {
        method: "ANNOUNCE",
        uri: &uri,
        headers: vec![],
        body: Some(("application/sdp", sdp.as_bytes())),
    })?;
    if response.status != 200 {
        return Err(Failure::AnnounceRefused(response));
    }

    let response = exchange(Request {
        method: "SETUP",
        uri: &uri,
        headers: vec![(
            "Transport",
            format!(
                "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={};timing_port={}",
                offer.control_port, offer.timing_port
            ),
        )],
        body: None,
    })?;
    let response = expect_ok("SETUP", response)?;
    let session = response
        .header("Session")
        .and_then(|s| s.split(';').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Failure::Other("its SETUP answer has no Session".into()))?
        .to_owned();
    let transport = response.header("Transport").unwrap_or_default();
    let port = |key| {
        transport_port(transport, key).ok_or_else(|| {
            Failure::Other(format!(
                "its SETUP answer has no {key} in Transport {transport:?}"
            ))
        })
    };
    let (mut audio, mut control) = (peer, peer);
    audio.set_port(port("server_port")?);
    control.set_port(port("control_port")?);
    connection.add_header("Session", session);

    let response = connection
        .exchange(
            &Request {
                method: "RECORD",
                uri: &uri,
                headers: vec![
                    ("Range", "npt=0-".into()),
                    (
                        "RTP-Info",
                        format!("seq={};rtptime={}", offer.first_seq, offer.first_rtp),
                    ),
                ],
                body: None,
            },
            timeout,
            abort,
        )
        .map_err(Failure::Other)?;
    let response = expect_ok("RECORD", response)?;
    Ok(Session {
        connection,
        uri,
        audio,
        control,
        audio_latency: audio_latency(&response),
    })
}

fn audio_latency(record: &Response) -> AudioLatency {
    match record.header("Audio-Latency").and_then(|l| l.parse().ok()) {
        Some(frames) => AudioLatency::Reported(frames),
        None => AudioLatency::Assumed(ASSUMED_AUDIO_LATENCY),
    }
}

const AUTH_SETUP_UNENCRYPTED: u8 = 0x01;

fn sdp(session_id: u32, local: IpAddr, peer: IpAddr) -> String {
    let family = |ip: IpAddr| if ip.is_ipv4() { "IP4" } else { "IP6" };
    format!(
        "v=0\r\n\
         o=iTunes {session_id} 0 IN {} {local}\r\n\
         s=iTunes\r\n\
         c=IN {} {peer}\r\n\
         t=0 0\r\n\
         m=audio 0 RTP/AVP 96\r\n\
         a=rtpmap:96 AppleLossless\r\n\
         a=fmtp:96 {FRAMES_PER_PACKET} 0 16 40 10 14 2 255 0 0 {SAMPLE_RATE}\r\n",
        family(local),
        family(peer),
    )
}

fn transport_port(transport: &str, key: &str) -> Option<u16> {
    transport
        .split(';')
        .filter_map(|part| part.split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(key))
        .and_then(|(_, v)| v.trim().parse().ok())
}

fn refusal(method: &str, response: &Response) -> String {
    let status = format!("{} {}", response.status, response.reason);
    match response.status {
        401 => format!("it requires a password ({method} answered {status})"),
        453 => format!("it is busy with another sender ({method} answered {status})"),
        _ => format!("it refused {method} with {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receiver::tests::{MOVE_2_RAOP, SHAIRPORT_SYNC_RAOP, txt};

    #[test]
    fn the_announce_describes_uncompressed_alac_at_44100() {
        assert_eq!(
            sdp(
                42,
                "192.0.2.1".parse().unwrap(),
                "192.0.2.2".parse().unwrap()
            ),
            "v=0\r\no=iTunes 42 0 IN IP4 192.0.2.1\r\ns=iTunes\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\n\
             m=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\na=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n"
        );
        assert!(
            sdp(1, "fe80::1".parse().unwrap(), "fe80::2".parse().unwrap())
                .contains("IN IP6 fe80::2")
        );
    }

    #[test]
    fn setup_answers_give_the_receiver_ports() {
        let transport =
            "RTP/AVP/UDP;unicast;mode=record;server_port=6001;control_port=6002;timing_port=6003";
        assert_eq!(transport_port(transport, "server_port"), Some(6001));
        assert_eq!(transport_port(transport, "control_port"), Some(6002));
        assert_eq!(transport_port("RTP/AVP/UDP;unicast", "server_port"), None);
    }

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

    #[test]
    fn auth_setup_is_retried_only_for_receivers_advertising_mfi_that_refuse() {
        assert!(retry_with_auth_setup(403, &txt(MOVE_2_RAOP)));
        assert!(retry_with_auth_setup(406, &txt(MOVE_2_RAOP)));
        assert!(!retry_with_auth_setup(401, &txt(MOVE_2_RAOP)));
        assert!(!retry_with_auth_setup(453, &txt(MOVE_2_RAOP)));
        assert!(!retry_with_auth_setup(403, &txt(SHAIRPORT_SYNC_RAOP)));
    }
}
