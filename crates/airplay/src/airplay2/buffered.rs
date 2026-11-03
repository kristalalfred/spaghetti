use std::io::ErrorKind;
use std::net::{IpAddr, TcpStream};
use std::time::{Duration, Instant};

use plist::Value;
use tracing::debug;

use super::{
    ALAC_44100_16_2, COMPRESSION_ALAC, Failure, Paired, binary, device_id, dictionary, open_events,
    pair, port, random_uuid, refused, session_teardown, setup_turned_away_as, stream_teardown,
    teardown, with_port,
};
use crate::events::EventChannel;
use crate::hap::{Counted, KEY_LEN};
use crate::ptp::{ClockIdentity, PtpTime, Timeline};
use crate::rtsp::{self, Connection, PLIST, Request, Response};
use crate::session::{Session, StreamRef, Transport, refusal};
use crate::stream::pause_until;
use crate::{FRAMES_PER_PACKET, SAMPLE_RATE};

const BUFFERED: u64 = 103;
/// Claude: how soon an anchor the receiver turned away with 400, as receivers
/// do until they have measured this host's clock, is offered again.
const ANCHOR_RETRY: Duration = Duration::from_millis(250);
const PEER_LIST: &str = "/peer-list-changed";

/// Claude: the audio format of buffered streams, as their stream SETUP names
/// it and each packet's SSRC carries it. ALAC at 44.1 kHz, 16-bit and the
/// renderer's 352 frames is an experiment: no receiver has yet been heard
/// playing it, and its SSRC is inferred (`Format::ssrc`). Apple's sender sent
/// the soundbar AAC-LC at 44.1 kHz.
pub(crate) const FORMAT: Format = Format {
    audio_format: ALAC_44100_16_2,
    compression: COMPRESSION_ALAC,
};

pub(crate) struct Format {
    pub(crate) audio_format: u64,
    pub(crate) compression: u64,
}

impl Format {
    /// Claude: the index of the format's `audioFormat` bit shifted left 24,
    /// as every known code is: 0x16000000 for AAC-LC at 44.1 kHz (bit 22), as
    /// Apple's sender was captured sending, 0x17000000 for AAC-LC at 48 kHz
    /// (bit 23) and 0x15000000 for ALAC at 48 kHz/24-bit (bit 21), as
    /// shairport-sync reads them.
    pub(crate) const fn ssrc(&self) -> u32 {
        self.audio_format.trailing_zeros() << 24
    }
}

/// Claude: this host's clock as a buffered session names it to the receiver:
/// its identity, and the address it times from, which is the session's.
pub(crate) struct TimingPeer {
    pub(crate) clock: ClockIdentity,
    pub(crate) address: IpAddr,
}

/// Claude: the receiver plays the frame with RTP time `rtp` when its
/// timeline, `timeline`, reads `network`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Anchor {
    pub(crate) rtp: u32,
    pub(crate) network: PtpTime,
    pub(crate) timeline: Timeline,
}

/// Claude: a buffered stream's data connection, and the stream as its
/// TEARDOWN names it.
pub(crate) struct DataConnection {
    socket: TcpStream,
    stream: StreamRef,
}

/// Claude: what a receiver accepted for a buffered stream: the data
/// connection its packets go through, the key that seals them, and the
/// anchor it took and when its answer came.
pub(crate) struct BufferedAudio {
    pub(crate) data: TcpStream,
    pub(crate) key: Counted,
    pub(crate) anchor: Anchor,
    pub(crate) anchored_at: Instant,
}

/// Claude: a buffered session being set up, holding what `abandon` tears
/// down: a session set up with PTP timing, and its stream once set up.
pub(crate) struct Handshake {
    connection: Connection,
    uri: String,
    events: Option<EventChannel>,
    audio_key: [u8; KEY_LEN],
    session_id: u32,
    answered_at: Instant,
    stream: Option<StreamRef>,
}

/// Claude: pairs, sets the session up with PTP timing naming this host's
/// clock as its timing peer, opens the receiver's event connection, records,
/// and names this host and the receiver as the session's peers, in the order
/// OwnTone and other buffered senders use, since Samsung, Sonos and TV
/// receivers are reported to play buffered audio only when RECORD comes
/// before the stream's SETUP. A handshake that fails after the session's
/// SETUP tears the session down within `teardown_timeout`.
pub(crate) fn set_up(
    connection: Connection,
    peer: &TimingPeer,
    timeout: Duration,
    teardown_timeout: Duration,
    abort: &dyn Fn() -> bool,
) -> Result<Handshake, Failure> {
    let Paired {
        mut connection,
        uri,
        session_id,
        events_keys,
        audio_key,
    } = pair(connection, timeout, abort)?;
    let receiver = connection.peer_addr();
    let reply = setup_turned_away_as(
        &mut connection,
        &uri,
        session_setup(peer)?,
        timeout,
        abort,
        turned_away,
    )?;
    let mut handshake = Handshake {
        connection,
        uri,
        events: None,
        audio_key,
        session_id,
        answered_at: Instant::now(),
        stream: None,
    };
    debug!("AirPlay 2 session SETUP with PTP timing answered");
    let peers = peer_list(receiver.ip(), peer.address);
    let recorded = open_events(receiver, &reply, &events_keys, timeout, abort).and_then(|events| {
        handshake.events = events;
        for (method, body) in [
            ("RECORD", None),
            ("SETPEERS", Some((PEER_LIST, &peers[..]))),
        ] {
            let response = handshake.exchange(method, body, timeout, abort)?;
            if response.status != 200 {
                return Err(refused(method, &response));
            }
        }
        Ok(())
    });
    match recorded {
        Ok(()) => Ok(handshake),
        Err(failure) => {
            handshake.abandon(teardown_timeout, abort);
            Err(failure)
        }
    }
}

impl Handshake {
    /// Claude: when the session's SETUP was answered, from which the startup
    /// budget runs.
    pub(crate) fn answered_at(&self) -> Instant {
        self.answered_at
    }

    /// Claude: sets the buffered stream up and connects to its data port.
    pub(crate) fn open_stream(
        &mut self,
        timeout: Duration,
        abort: &dyn Fn() -> bool,
    ) -> Result<DataConnection, Failure> {
        let reply = setup_turned_away_as(
            &mut self.connection,
            &self.uri,
            stream_setup(&self.audio_key, self.session_id),
            timeout,
            abort,
            turned_away,
        )?;
        let stream = reply
            .get("streams")
            .and_then(Value::as_array)
            .and_then(|streams| streams.first())
            .and_then(Value::as_dictionary)
            .ok_or_else(|| Failure::Failed("its stream SETUP answer has no stream".into()))?;
        let set_up = StreamRef {
            kind: BUFFERED,
            id: stream.get("streamID").and_then(Value::as_unsigned_integer),
        };
        self.stream = Some(set_up);
        let data_at = with_port(self.connection.peer_addr(), port(stream, "dataPort")?);
        debug!(%data_at, "AirPlay 2 buffered stream SETUP answered");
        let socket = rtsp::connect(data_at, timeout, abort).map_err(|e| {
            Failure::Failed(match e.kind() {
                ErrorKind::Interrupted => "stopped".into(),
                _ => format!("could not connect to its data port {data_at} ({e})"),
            })
        })?;
        debug!(%data_at, "AirPlay 2 data connection opened");
        Ok(DataConnection {
            socket,
            stream: set_up,
        })
    }

    /// Claude: offers the receiver anchors, each made by `anchor` as it goes
    /// out, until it accepts one: a 400 is offered again after `ANCHOR_RETRY`
    /// while that is before `deadline`, and once it no longer is, buffered
    /// audio does not start. Any other answer but 200 ends the handshake as
    /// `turned_away` says; no answer within `timeout`, or a malformed one,
    /// fails it. Returns the accepted anchor and when its answer came.
    pub(crate) fn anchor(
        &mut self,
        deadline: Instant,
        timeout: Duration,
        abort: &dyn Fn() -> bool,
        anchor: &mut dyn FnMut() -> Result<Anchor, Failure>,
    ) -> Result<(Anchor, Instant), Failure> {
        let mut refusals = 0u32;
        loop {
            let offered = anchor()?;
            let body = anchor_body(&offered);
            let response =
                self.exchange("SETRATEANCHORTIME", Some((PLIST, &body)), timeout, abort)?;
            let answered_at = Instant::now();
            match response.status {
                200 => {
                    debug!(refusals, ?offered, "AirPlay 2 anchor accepted");
                    return Ok((offered, answered_at));
                }
                400 if answered_at + ANCHOR_RETRY < deadline => {
                    refusals += 1;
                    debug!(refusals, "AirPlay 2 receiver not ready for an anchor yet");
                    if !pause_until(answered_at + ANCHOR_RETRY, abort) {
                        return Err(Failure::Failed("stopped".into()));
                    }
                }
                400 => {
                    return Err(Failure::Unbuffered(format!(
                        "it did not accept an anchor before the startup budget ran out ({})",
                        refusal("SETRATEANCHORTIME", &response)
                    )));
                }
                _ => return Err(turned_away("SETRATEANCHORTIME", &response)),
            }
        }
    }

    /// Claude: the session, to stream through `data` from `anchor` on.
    pub(crate) fn start(
        self,
        data: DataConnection,
        anchor: Anchor,
        anchored_at: Instant,
    ) -> (Session, BufferedAudio) {
        let Handshake {
            connection,
            uri,
            events,
            audio_key,
            ..
        } = self;
        (
            Session {
                connection,
                uri,
                transport: Transport::AirPlay2 {
                    _events: events,
                    stream: data.stream,
                },
            },
            BufferedAudio {
                data: data.socket,
                key: Counted::new(&audio_key),
                anchor,
                anchored_at,
            },
        )
    }

    /// Claude: tears down the stream, when it was set up, then the session,
    /// within `timeout`; nothing when stopped, which closing the connection
    /// ends anyway.
    pub(crate) fn abandon(mut self, timeout: Duration, abort: &dyn Fn() -> bool) {
        if abort() {
            return;
        }
        let deadline = Instant::now() + timeout;
        let bodies = self
            .stream
            .map(stream_teardown)
            .into_iter()
            .chain([session_teardown()]);
        for body in bodies {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self
                .connection
                .exchange(&teardown(&self.uri, &body), remaining, abort)
            {
                Ok(response) => debug!(status = response.status, "TEARDOWN answered"),
                Err(reason) => {
                    debug!(%reason, "TEARDOWN unanswered");
                    return;
                }
            }
        }
    }

    fn exchange(
        &mut self,
        method: &str,
        body: Option<(&'static str, &[u8])>,
        timeout: Duration,
        abort: &dyn Fn() -> bool,
    ) -> Result<Response, Failure> {
        let request = Request {
            method,
            uri: &self.uri,
            headers: vec![],
            body,
        };
        self.connection
            .exchange(&request, timeout, abort)
            .map_err(Failure::Failed)
    }
}

/// Claude: what a buffered step's answer other than 200 means: a password, a
/// PIN or another sender's session refuse the stream as they refuse any
/// step, and any other status says the receiver does not take buffered audio
/// this way.
fn turned_away(method: &str, response: &Response) -> Failure {
    match response.status {
        401 | 403 | 453 | 470 => Failure::Refused(refusal(method, response)),
        _ => Failure::Unbuffered(refusal(method, response)),
    }
}

/// Claude: the session's SETUP with PTP timing, with the keys Apple's and
/// other buffered senders send. This host's clock is the timing peer, once
/// as `timingPeerInfo` and once in `timingPeerList`; its identity goes as
/// the signed integer iOS sends.
fn session_setup(peer: &TimingPeer) -> Result<Vec<u8>, Failure> {
    let device_id = device_id()?;
    let timing_peer = dictionary([
        (
            "Addresses",
            vec![Value::from(peer.address.to_string())].into(),
        ),
        ("ID", random_uuid()?.into()),
        ("DeviceType", 0u64.into()),
        ("ClockID", signed(peer.clock.as_u64())),
        ("SupportsClockPortMatchingOverride", false.into()),
    ]);
    Ok(binary(dictionary([
        ("deviceID", device_id.clone().into()),
        ("macAddress", device_id.into()),
        ("sessionUUID", random_uuid()?.into()),
        ("groupUUID", random_uuid()?.into()),
        ("groupContainsGroupLeader", false.into()),
        ("timingProtocol", "PTP".into()),
        ("timingPeerInfo", timing_peer.clone()),
        ("timingPeerList", vec![timing_peer].into()),
    ])))
}

/// Claude: SETPEERS's body: the receiver's address and this host's.
fn peer_list(receiver: IpAddr, ours: IpAddr) -> Vec<u8> {
    binary(Value::Array(vec![
        receiver.to_string().into(),
        ours.to_string().into(),
    ]))
}

/// Claude: one buffered stream in `FORMAT`, without the `latencyMin` and
/// `latencyMax` a realtime stream asks for: Apple's sender omits them, and
/// Sonos receivers are reported to add `latencyMax` to the anchor.
fn stream_setup(audio_key: &[u8; KEY_LEN], session_id: u32) -> Vec<u8> {
    let stream = dictionary([
        ("audioFormat", FORMAT.audio_format.into()),
        ("audioMode", "default".into()),
        ("audioType", "media".into()),
        ("ct", FORMAT.compression.into()),
        ("isMedia", true.into()),
        ("shk", Value::Data(audio_key.to_vec())),
        ("spf", (FRAMES_PER_PACKET as u64).into()),
        ("sr", u64::from(SAMPLE_RATE).into()),
        ("supportsDynamicStreamID", false.into()),
        ("streamConnectionID", u64::from(session_id).into()),
        ("type", BUFFERED.into()),
    ]);
    binary(dictionary([("streams", vec![stream].into())]))
}

/// Claude: SETRATEANCHORTIME's body, playing (`rate` 1) from the anchor on.
/// 64-bit values go as signed integers of the same bits, which a receiver
/// reading them signed or unsigned takes alike.
fn anchor_body(anchor: &Anchor) -> Vec<u8> {
    binary(dictionary([
        ("networkTimeSecs", anchor.network.secs().into()),
        ("networkTimeFrac", signed(anchor.network.frac())),
        (
            "networkTimeTimelineID",
            signed(anchor.timeline.clock.as_u64()),
        ),
        ("rate", 1u64.into()),
        ("rtpTime", u64::from(anchor.rtp).into()),
    ]))
}

fn signed(value: u64) -> Value {
    Value::Integer((value as i64).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist::Dictionary;

    fn read_plist(bytes: &[u8]) -> Value {
        Value::from_reader(std::io::Cursor::new(bytes)).unwrap()
    }

    fn dictionary_of(bytes: &[u8]) -> Dictionary {
        read_plist(bytes).into_dictionary().unwrap()
    }

    fn unsigned(dictionary: &Dictionary, key: &str) -> u64 {
        dictionary[key]
            .as_signed_integer()
            .map(|value| value as u64)
            .unwrap_or_else(|| panic!("{key} is an integer"))
    }

    #[test]
    fn format_codes_are_the_audio_format_bit_shifted_left_24() {
        let code = |audio_format| {
            Format {
                audio_format,
                compression: 0,
            }
            .ssrc()
        };
        assert_eq!(code(0x40_0000), 0x1600_0000, "AAC-LC 44.1 kHz, as captured");
        assert_eq!(code(0x80_0000), 0x1700_0000, "AAC-LC 48 kHz");
        assert_eq!(code(0x20_0000), 0x1500_0000, "ALAC 48 kHz/24-bit");
        assert_eq!(FORMAT.ssrc(), 0x1200_0000);
        assert_eq!(
            (FORMAT.audio_format, FORMAT.compression),
            (0x40000, 2),
            "ALAC 44.1 kHz/16-bit"
        );
    }

    #[test]
    fn the_session_setup_names_ptp_and_this_hosts_clock_as_its_timing_peer() {
        let peer = TimingPeer {
            clock: ClockIdentity([0xB8, 0xAB, 0x62, 0xFF, 0xFE, 0xC6, 0x1A, 0xDF]),
            address: "fe80::1c2d:3e4f:5a6b:7c8d".parse().unwrap(),
        };
        let setup = dictionary_of(&session_setup(&peer).unwrap());
        assert_eq!(setup["timingProtocol"].as_string(), Some("PTP"));
        assert_eq!(setup["groupContainsGroupLeader"].as_boolean(), Some(false));
        let device = setup["deviceID"].as_string().unwrap();
        assert_eq!((device.len(), device.matches(':').count()), (17, 5));
        assert_eq!(setup["macAddress"].as_string(), Some(device));
        for key in ["sessionUUID", "groupUUID"] {
            assert_eq!(setup[key].as_string().unwrap().len(), 36, "{key}");
        }
        assert_ne!(setup["sessionUUID"], setup["groupUUID"]);
        let info = setup["timingPeerInfo"].as_dictionary().unwrap();
        assert_eq!(
            info["Addresses"].as_array().unwrap(),
            &[Value::from("fe80::1c2d:3e4f:5a6b:7c8d")]
        );
        assert_eq!(info["ID"].as_string().unwrap().len(), 36);
        assert_eq!(unsigned(info, "DeviceType"), 0);
        assert_eq!(unsigned(info, "ClockID"), 0xB8AB_62FF_FEC6_1ADF);
        assert_eq!(
            info["ClockID"].as_signed_integer(),
            Some(0xB8AB_62FF_FEC6_1ADFu64 as i64)
        );
        assert_eq!(
            info["SupportsClockPortMatchingOverride"].as_boolean(),
            Some(false)
        );
        assert_eq!(
            setup["timingPeerList"].as_array().unwrap(),
            &[Value::Dictionary(info.clone())]
        );
        assert!(!setup.contains_key("timingPort"));
        assert_eq!(setup.len(), 8);
    }

    #[test]
    fn the_peer_list_names_the_receiver_then_this_host() {
        let list = read_plist(&peer_list(
            "192.168.3.57".parse().unwrap(),
            "192.168.3.10".parse().unwrap(),
        ));
        assert_eq!(
            list.as_array().unwrap(),
            &[Value::from("192.168.3.57"), Value::from("192.168.3.10")]
        );
    }

    #[test]
    fn the_stream_setup_asks_for_buffered_audio_in_the_format_without_latencies() {
        let setup = dictionary_of(&stream_setup(&[7; 32], 1234));
        let stream = setup["streams"].as_array().unwrap()[0]
            .as_dictionary()
            .unwrap();
        let number = |key: &str| stream[key].as_unsigned_integer().unwrap();
        assert_eq!(
            [
                "type",
                "ct",
                "audioFormat",
                "spf",
                "sr",
                "streamConnectionID"
            ]
            .map(number),
            [103, 2, 0x40000, 352, 44_100, 1234]
        );
        assert_eq!(stream["shk"].as_data(), Some(&[7; 32][..]));
        assert_eq!(stream["audioType"].as_string(), Some("media"));
        for absent in ["latencyMin", "latencyMax", "controlPort"] {
            assert!(!stream.contains_key(absent), "{absent}");
        }
    }

    #[test]
    fn the_anchor_plays_its_rtp_time_at_its_network_time_on_its_timeline() {
        let anchor = Anchor {
            rtp: 0xFFFF_FF00,
            network: PtpTime {
                nanos: 231_017_750_000_000,
            },
            timeline: Timeline {
                clock: ClockIdentity([0xB8, 0xAB, 0x62, 0xFF, 0xFE, 0xC6, 0x1A, 0xDF]),
                epoch: 3,
            },
        };
        let body = dictionary_of(&anchor_body(&anchor));
        assert_eq!(unsigned(&body, "rate"), 1);
        assert_eq!(unsigned(&body, "rtpTime"), 0xFFFF_FF00);
        assert_eq!(unsigned(&body, "networkTimeSecs"), 231_017);
        assert_eq!(unsigned(&body, "networkTimeFrac"), 3 << 62);
        assert_eq!(
            unsigned(&body, "networkTimeTimelineID"),
            0xB8AB_62FF_FEC6_1ADF
        );
        assert_eq!(body.len(), 5);
    }

    #[test]
    fn a_password_a_pin_or_a_busy_receiver_refuses_and_any_other_status_unbuffers() {
        let answered = |status| Response {
            status,
            reason: "Whatever".into(),
            headers: vec![],
            body: vec![],
        };
        for status in [401, 403, 453, 470] {
            assert!(
                matches!(turned_away("SETUP", &answered(status)), Failure::Refused(_)),
                "{status}"
            );
        }
        for status in [400, 404, 455, 500, 501] {
            assert!(
                matches!(
                    turned_away("SETUP", &answered(status)),
                    Failure::Unbuffered(_)
                ),
                "{status}"
            );
        }
    }

    mod handshakes {
        use std::io::Write;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;

        use super::*;
        use crate::fake::{Behavior, Fake, FakeRequest, Rtsp, Step, wait_until};
        use crate::packet::{BUFFERED_PACKET_LEN, write_buffered_packet};

        /// Claude: room for the fake's SRP in a debug build to answer pairing.
        const TIMEOUT: Duration = Duration::from_secs(3);
        const TEARDOWN: Duration = Duration::from_millis(300);
        const TIMELINE: Timeline = Timeline {
            clock: ClockIdentity([0xB8, 0xAB, 0x62, 0xFF, 0xFE, 0xC6, 0x1A, 0xDF]),
            epoch: 0,
        };

        fn set_up_with(fake: &Fake, abort: &dyn Fn() -> bool) -> Result<Handshake, Failure> {
            let connection = Connection::open(
                &[fake.address()],
                TIMEOUT,
                Instant::now() + TIMEOUT,
                vec![("User-Agent", super::super::super::USER_AGENT.into())],
                abort,
            )
            .unwrap_or_else(|e| panic!("{e}"));
            let peer = TimingPeer {
                clock: ClockIdentity([1, 2, 3, 4, 5, 6, 7, 8]),
                address: "127.0.0.1".parse().unwrap(),
            };
            set_up(connection, &peer, TIMEOUT, TEARDOWN, abort)
        }

        fn last(fake: &Fake, method: &str) -> FakeRequest {
            fake.requests()
                .into_iter()
                .map(|(_, _, request)| request)
                .rfind(|request| request.method == method)
                .unwrap()
        }

        /// Claude: anchors one second apart on `TIMELINE`, counting how many
        /// it made.
        fn anchors(made: &mut u64) -> impl FnMut() -> Result<Anchor, Failure> + '_ {
            move || {
                *made += 1;
                Ok(Anchor {
                    rtp: 1000,
                    network: PtpTime {
                        nanos: 5_000_000_000 * *made,
                    },
                    timeline: TIMELINE,
                })
            }
        }

        fn teardown_bodies(fake: &Fake) -> Vec<Dictionary> {
            fake.requests()
                .into_iter()
                .filter(|(_, _, request)| request.method == "TEARDOWN")
                .map(|(_, _, request)| request.plist())
                .collect()
        }

        #[test]
        fn a_buffered_session_records_names_its_peers_sets_up_its_stream_then_anchors() {
            let fake = Fake::start(Behavior {
                anchor_refusals: 2,
                ..Behavior::default()
            });
            let mut handshake = set_up_with(&fake, &|| false).unwrap();
            let data = handshake.open_stream(TIMEOUT, &|| false).unwrap();
            let mut made = 0;
            let deadline = handshake.answered_at() + Duration::from_secs(8);
            let started = Instant::now();
            let (anchor, anchored_at) = handshake
                .anchor(deadline, TIMEOUT, &|| false, &mut anchors(&mut made))
                .unwrap();
            assert!(anchored_at - started >= 2 * ANCHOR_RETRY);
            assert_eq!(made, 3, "a fresh anchor for each offer");
            assert_eq!(anchor.network.nanos, 15_000_000_000);
            let (mut session, mut audio) = handshake.start(data, anchor, anchored_at);

            assert_eq!(
                fake.methods(),
                [
                    "GET",
                    "POST",
                    "POST",
                    "SETUP",
                    "RECORD",
                    "SETPEERS",
                    "SETUP",
                    "SETRATEANCHORTIME",
                    "SETRATEANCHORTIME",
                    "SETRATEANCHORTIME"
                ]
            );
            let peers = last(&fake, "SETPEERS");
            assert_eq!(peers.header("Content-Type"), Some("/peer-list-changed"));
            assert_eq!(
                read_plist(&peers.body).as_array().unwrap(),
                &[Value::from("127.0.0.1"), Value::from("127.0.0.1")]
            );
            let offered = fake.anchors();
            assert_eq!(offered.len(), 3);
            for pair in offered.windows(2) {
                assert!(pair[1].0 - pair[0].0 >= ANCHOR_RETRY);
            }
            let accepted = &offered[2].1;
            assert_eq!(unsigned(accepted, "networkTimeSecs"), 15);
            assert_eq!(unsigned(accepted, "rtpTime"), 1000);
            assert_eq!(
                last(&fake, "SETRATEANCHORTIME").header("Content-Type"),
                Some(PLIST)
            );

            let silence = [0.0; FRAMES_PER_PACKET];
            for n in 0..3u32 {
                let mut packet = [0; BUFFERED_PACKET_LEN];
                let len = write_buffered_packet(
                    &mut packet,
                    n,
                    1000 + n * 352,
                    FORMAT.ssrc(),
                    &silence,
                    &silence,
                    &mut audio.key,
                );
                audio.data.write_all(&packet[..len]).unwrap();
            }
            assert!(wait_until(Duration::from_secs(1), || fake
                .buffered_audio()
                .len()
                == 3));
            let received = fake.buffered_audio();
            assert_eq!(received[2].1.timestamp, 1000 + 2 * 352);
            assert_eq!(received[2].1.ssrc, 0x1200_0000);

            session.teardown(TEARDOWN);
            let bodies = teardown_bodies(&fake);
            let stream = bodies[0]["streams"].as_array().unwrap()[0]
                .as_dictionary()
                .unwrap();
            assert_eq!(unsigned(stream, "type"), 103);
            assert_eq!(unsigned(stream, "streamID"), crate::fake::STREAM_ID);
            assert!(bodies[1].is_empty());
        }

        #[test]
        fn anchors_turned_away_until_the_budget_ends_unbuffer_and_abandoning_tears_both_down() {
            let fake = Fake::start(Behavior {
                anchor_refusals: u32::MAX,
                ..Behavior::default()
            });
            let mut handshake = set_up_with(&fake, &|| false).unwrap();
            let _data = handshake.open_stream(TIMEOUT, &|| false).unwrap();
            let mut made = 0;
            let deadline = Instant::now() + 3 * ANCHOR_RETRY;
            let failure = handshake
                .anchor(deadline, TIMEOUT, &|| false, &mut anchors(&mut made))
                .unwrap_err();
            let ended = Instant::now();
            assert!(
                matches!(&failure, Failure::Unbuffered(reason) if reason.contains("400")),
                "{failure:?}"
            );
            assert!(ended <= deadline, "no offer is made past the budget");
            assert_eq!(made, 3);
            handshake.abandon(TEARDOWN, &|| false);
            let bodies = teardown_bodies(&fake);
            assert_eq!(bodies.len(), 2);
            let stream = bodies[0]["streams"].as_array().unwrap()[0]
                .as_dictionary()
                .unwrap();
            assert_eq!(unsigned(stream, "type"), 103);
            assert!(bodies[1].is_empty());
        }

        #[test]
        fn anchor_answers_other_than_200_and_400_refuse_unbuffer_or_fail() {
            for (behavior, expected) in [
                (Rtsp::RefuseAt(Step::Anchor, 501), "Unbuffered"),
                (Rtsp::RefuseAt(Step::Anchor, 455), "Unbuffered"),
                (Rtsp::RefuseAt(Step::Anchor, 453), "Refused"),
                (Rtsp::RefuseAt(Step::Anchor, 401), "Refused"),
                (Rtsp::IgnoreAt(Step::Anchor), "Failed"),
            ] {
                let fake = Fake::start(Behavior {
                    rtsp: behavior,
                    ..Behavior::default()
                });
                let mut handshake = set_up_with(&fake, &|| false).unwrap();
                let _data = handshake.open_stream(TIMEOUT, &|| false).unwrap();
                let mut made = 0;
                let failure = handshake
                    .anchor(
                        Instant::now() + Duration::from_secs(8),
                        Duration::from_millis(400),
                        &|| false,
                        &mut anchors(&mut made),
                    )
                    .unwrap_err();
                let kind = match failure {
                    Failure::Unbuffered(_) => "Unbuffered",
                    Failure::Refused(_) => "Refused",
                    Failure::Failed(_) => "Failed",
                };
                assert_eq!((kind, made), (expected, 1), "{behavior:?}: {failure:?}");
            }
        }

        #[test]
        fn a_stream_waiting_to_offer_its_anchor_again_stops_promptly() {
            let fake = Fake::start(Behavior {
                anchor_refusals: u32::MAX,
                ..Behavior::default()
            });
            let stopped = AtomicBool::new(false);
            let abort = || stopped.load(Ordering::Acquire);
            let mut handshake = set_up_with(&fake, &abort).unwrap();
            let _data = handshake.open_stream(TIMEOUT, &abort).unwrap();
            let mut made = 0;
            let (failure, returned, stopped_at) = thread::scope(|scope| {
                let stopper = scope.spawn(|| {
                    assert!(wait_until(TIMEOUT, || !fake.anchors().is_empty()));
                    thread::sleep(ANCHOR_RETRY / 4);
                    stopped.store(true, Ordering::Release);
                    Instant::now()
                });
                let failure = handshake
                    .anchor(
                        Instant::now() + Duration::from_secs(8),
                        TIMEOUT,
                        &abort,
                        &mut anchors(&mut made),
                    )
                    .unwrap_err();
                (failure, Instant::now(), stopper.join().unwrap())
            });
            assert_eq!(failure, Failure::Failed("stopped".into()));
            assert_eq!(made, 1, "stopped while waiting to offer again");
            assert!(
                returned - stopped_at < Duration::from_millis(50),
                "{:?}",
                returned - stopped_at
            );
            handshake.abandon(TEARDOWN, &abort);
            assert!(
                teardown_bodies(&fake).is_empty(),
                "stopping sends nothing more"
            );
        }

        #[test]
        fn setup_answers_other_than_200_refuse_or_unbuffer_and_release_what_was_set_up() {
            for (step, status, expected) in [
                (Step::SessionSetup, 501, "Unbuffered"),
                (Step::SessionSetup, 500, "Unbuffered"),
                (Step::SessionSetup, 453, "Refused"),
                (Step::SessionSetup, 470, "Refused"),
                (Step::StreamSetup, 501, "Unbuffered"),
                (Step::StreamSetup, 403, "Refused"),
                (Step::Record, 501, "Refused"),
                (Step::SetPeers, 501, "Refused"),
            ] {
                let fake = Fake::start(Behavior {
                    rtsp: Rtsp::RefuseAt(step, status),
                    ..Behavior::default()
                });
                let failure = set_up_with(&fake, &|| false)
                    .and_then(
                        |mut handshake| match handshake.open_stream(TIMEOUT, &|| false) {
                            Ok(_) => panic!("{step:?} {status} was not refused"),
                            Err(failure) => {
                                handshake.abandon(TEARDOWN, &|| false);
                                Err::<(), _>(failure)
                            }
                        },
                    )
                    .unwrap_err();
                let kind = match failure {
                    Failure::Unbuffered(_) => "Unbuffered",
                    Failure::Refused(_) => "Refused",
                    Failure::Failed(_) => "Failed",
                };
                assert_eq!(kind, expected, "{step:?} {status}: {failure:?}");
                let bodies = teardown_bodies(&fake);
                if step == Step::SessionSetup {
                    assert!(bodies.is_empty(), "{step:?}: nothing to tear down");
                } else {
                    assert_eq!(bodies.len(), 1, "{step:?}: the session only");
                    assert!(bodies[0].is_empty());
                }
            }
        }
    }
}
