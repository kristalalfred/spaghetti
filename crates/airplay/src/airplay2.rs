use std::io::Cursor;
use std::net::SocketAddr;
use std::time::Duration;

use plist::{Dictionary, Value};
use tracing::{debug, warn};

use crate::events::EventChannel;
use crate::hap::{self, Counted, KEY_LEN};
use crate::pairing;
use crate::rtsp::{Connection, PLIST, Request, Response};
use crate::session::{
    self, ASSUMED_AUDIO_LATENCY, Offer, Packets, RealtimeAudio, Session, StreamRef, Transport,
    refusal,
};
use crate::{FRAMES_PER_PACKET, SAMPLE_RATE};

pub(crate) mod buffered;

/// Claude: how often iOS, and so pyatv, posts `/feedback` to keep a session.
pub(crate) const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);
/// Claude: HomePod OS 27 refuses `/info` to a sender whose User-Agent is not
/// AirPlay's own; OwnTone sends this one since, and its users found it works
/// with HomePods, an Apple TV and other receivers (owntone-server issue 2042).
pub(crate) const USER_AGENT: &str = "AirPlay/999.0.0";
pub(crate) const REALTIME: u64 = 96;
/// Claude: ALAC, 44.1 kHz, 16-bit stereo, in AirPlay 2's audio format bits.
pub(crate) const ALAC_44100_16_2: u64 = 0x40000;
pub(crate) const COMPRESSION_ALAC: u64 = 2;
const LATENCY_MAX: u64 = 88_200;

/// Claude: why an AirPlay 2 handshake did not end in a session. Buffered
/// audio with PTP timing that could not start, here or at the receiver, may
/// still start as realtime audio; a receiver that refused a step may still
/// take AirPlay 1; one that failed, by not answering, answering malformed or
/// proving the wrong key, is not tried again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    Unbuffered(String),
    Refused(String),
    Failed(String),
}

impl Failure {
    pub(crate) fn reason(&self) -> &str {
        match self {
            Failure::Unbuffered(reason) | Failure::Refused(reason) | Failure::Failed(reason) => {
                reason
            }
        }
    }
}

/// Claude: a connection paired transiently and encrypted, with what the rest
/// of either handshake derives from the pairing.
struct Paired {
    connection: Connection,
    uri: String,
    session_id: u32,
    events_keys: hap::Keys,
    audio_key: [u8; KEY_LEN],
}

/// Claude: asks for the receiver's `/info`, as OwnTone and pyatv do first,
/// pairs transiently and encrypts the connection.
fn pair(
    mut connection: Connection,
    timeout: Duration,
    abort: &dyn Fn() -> bool,
) -> Result<Paired, Failure> {
    let info = Request {
        method: "GET",
        uri: "/info",
        headers: vec![],
        body: None,
    };
    let answer = connection
        .exchange(&info, timeout, abort)
        .map_err(Failure::Failed)?;
    if answer.status != 200 {
        debug!(status = answer.status, "the receiver gave no /info");
    }
    let secret = pairing::pair_transiently(&mut connection, timeout, abort)?;
    connection.encrypt(&hap::keys(
        &secret,
        "Control-Salt",
        "Control-Write-Encryption-Key",
        "Control-Read-Encryption-Key",
    ));
    let session_id = random_u32()?;
    let uri = session::uri(connection.local_addr().ip(), session_id);
    let events_keys = hap::keys(
        &secret,
        "Events-Salt",
        "Events-Read-Encryption-Key",
        "Events-Write-Encryption-Key",
    );
    let mut audio_key = [0; KEY_LEN];
    getrandom::fill(&mut audio_key).map_err(|e| Failure::Failed(e.to_string()))?;
    Ok(Paired {
        connection,
        uri,
        session_id,
        events_keys,
        audio_key,
    })
}

/// Claude: pairs, then sets up the session with NTP timing, opens the
/// receiver's event connection, sets up one realtime ALAC stream with a fresh
/// key, records, and flushes to the stream's first packet. A handshake that
/// fails after the session's SETUP tears the session down within
/// `teardown_timeout`.
pub(crate) fn negotiate(
    connection: Connection,
    offer: &Offer,
    timeout: Duration,
    teardown_timeout: Duration,
    abort: &dyn Fn() -> bool,
) -> Result<(Session, RealtimeAudio), Failure> {
    let Paired {
        mut connection,
        uri,
        session_id,
        events_keys,
        audio_key,
    } = pair(connection, timeout, abort)?;
    let peer = connection.peer_addr();
    let reply = setup(&mut connection, &uri, session_setup(offer)?, timeout, abort)?;
    debug!(
        timing_port = offer.timing_port,
        "AirPlay 2 session SETUP answered"
    );
    let opened = open_events(peer, &reply, &events_keys, timeout, abort).and_then(|events| {
        let stream = stream_setup(offer, &audio_key, session_id);
        open_stream(&mut connection, &uri, events, stream, timeout, abort)
    });
    let opened = opened
        .and_then(|stream| flush(&mut connection, &uri, offer, timeout, abort).map(|()| stream));
    match opened {
        Ok(stream) => Ok((
            Session {
                connection,
                uri,
                transport: Transport::AirPlay2 {
                    _events: stream.events,
                    stream: StreamRef {
                        kind: REALTIME,
                        id: stream.stream_id,
                    },
                },
            },
            RealtimeAudio {
                audio: with_port(peer, stream.audio_port),
                control: with_port(peer, stream.control_port),
                latency: session::audio_latency(&stream.record),
                packets: Packets::AirPlay2(Counted::new(&audio_key)),
            },
        )),
        Err(failure) => {
            if !abort() {
                let body = session_teardown();
                match connection.exchange(&teardown(&uri, &body), teardown_timeout, abort) {
                    Ok(response) => debug!(status = response.status, "TEARDOWN answered"),
                    Err(reason) => debug!(%reason, "TEARDOWN unanswered"),
                }
            }
            Err(failure)
        }
    }
}

/// Claude: opens the event connection the session's SETUP answer names. One
/// that fails is left out, and the stream goes on without it.
fn open_events(
    peer: SocketAddr,
    reply: &Dictionary,
    keys: &hap::Keys,
    timeout: Duration,
    abort: &dyn Fn() -> bool,
) -> Result<Option<EventChannel>, Failure> {
    let events_at = with_port(peer, port(reply, "eventPort")?);
    match EventChannel::connect(events_at, keys, timeout, abort) {
        Ok(events) => Ok(Some(events)),
        Err(reason) if abort() => Err(Failure::Failed(reason)),
        Err(reason) => {
            warn!(%reason, "AirPlay 2 receiver's event connection failed; streaming without it");
            Ok(None)
        }
    }
}

/// Claude: what a session's stream has once set up and recording.
struct OpenStream {
    events: Option<EventChannel>,
    audio_port: u16,
    control_port: u16,
    stream_id: Option<u64>,
    record: Response,
}

/// Claude: sets the stream up and records.
fn open_stream(
    connection: &mut Connection,
    uri: &str,
    events: Option<EventChannel>,
    stream: Vec<u8>,
    timeout: Duration,
    abort: &dyn Fn() -> bool,
) -> Result<OpenStream, Failure> {
    let reply = setup(connection, uri, stream, timeout, abort)?;
    let stream = reply
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|streams| streams.first())
        .and_then(Value::as_dictionary)
        .ok_or_else(|| Failure::Failed("its stream SETUP answer has no stream".into()))?;
    let (audio_port, control_port) = (port(stream, "dataPort")?, port(stream, "controlPort")?);
    let stream_id = stream.get("streamID").and_then(Value::as_unsigned_integer);
    debug!(audio_port, control_port, "AirPlay 2 stream SETUP answered");
    let record = exchange(
        connection,
        &Request {
            method: "RECORD",
            uri,
            headers: vec![],
            body: None,
        },
        timeout,
        abort,
    )?;
    debug!("AirPlay 2 RECORD answered");
    Ok(OpenStream {
        events,
        audio_port,
        control_port,
        stream_id,
        record,
    })
}

/// Claude: a FLUSH naming the stream's first packet, after RECORD and before
/// any packet: with it, an HK Citation that started 65 to 110 ms late and then
/// slewed in 21 of 30 audible sessions started on time in 19 of 20. A
/// receiver that refuses it or does not answer within `timeout` still gets
/// the stream; only `abort` ends the handshake here.
fn flush(
    connection: &mut Connection,
    uri: &str,
    offer: &Offer,
    timeout: Duration,
    abort: &dyn Fn() -> bool,
) -> Result<(), Failure> {
    let request = Request {
        method: "FLUSH",
        uri,
        headers: vec![
            ("Range", "npt=0-".to_owned()),
            (
                "RTP-Info",
                format!("seq={};rtptime={}", offer.first_seq, offer.first_rtp),
            ),
        ],
        body: None,
    };
    match connection.exchange(&request, timeout, abort) {
        Ok(response) if response.status == 200 => {
            debug!(status = response.status, "AirPlay 2 FLUSH answered");
        }
        Ok(response) => {
            warn!(
                status = response.status,
                "AirPlay 2 receiver refused the FLUSH; starting the stream anyway"
            );
        }
        Err(reason) if abort() => return Err(Failure::Failed(reason)),
        Err(reason) => {
            warn!(%reason, "AirPlay 2 FLUSH unanswered; starting the stream anyway");
        }
    }
    Ok(())
}

fn setup(
    connection: &mut Connection,
    uri: &str,
    body: Vec<u8>,
    timeout: Duration,
    abort: &dyn Fn() -> bool,
) -> Result<Dictionary, Failure> {
    setup_turned_away_as(connection, uri, body, timeout, abort, refused)
}

/// Claude: a SETUP whose answer other than 200 means what `turned_away` makes
/// of it.
fn setup_turned_away_as(
    connection: &mut Connection,
    uri: &str,
    body: Vec<u8>,
    timeout: Duration,
    abort: &dyn Fn() -> bool,
    turned_away: fn(&str, &Response) -> Failure,
) -> Result<Dictionary, Failure> {
    let request = Request {
        method: "SETUP",
        uri,
        headers: vec![],
        body: Some((PLIST, &body)),
    };
    let response = connection
        .exchange(&request, timeout, abort)
        .map_err(Failure::Failed)?;
    if response.status != 200 {
        return Err(turned_away(request.method, &response));
    }
    Value::from_reader(Cursor::new(&response.body))
        .ok()
        .and_then(Value::into_dictionary)
        .ok_or_else(|| Failure::Failed("its SETUP answer is not a plist dictionary".into()))
}

fn exchange(
    connection: &mut Connection,
    request: &Request,
    timeout: Duration,
    abort: &dyn Fn() -> bool,
) -> Result<Response, Failure> {
    let response = connection
        .exchange(request, timeout, abort)
        .map_err(Failure::Failed)?;
    if response.status != 200 {
        return Err(refused(request.method, &response));
    }
    Ok(response)
}

fn refused(method: &str, response: &Response) -> Failure {
    Failure::Refused(refusal(method, response))
}

fn port(dictionary: &Dictionary, key: &str) -> Result<u16, Failure> {
    dictionary
        .get(key)
        .and_then(Value::as_unsigned_integer)
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| Failure::Failed(format!("its SETUP answer has no {key}")))
}

fn with_port(mut address: SocketAddr, port: u16) -> SocketAddr {
    address.set_port(port);
    address
}

/// Claude: the session's SETUP with the fields OwnTone 28 sent for NTP timing,
/// which streamed to Sonos receivers advertising the HK Citation's feature
/// bits and source version.
fn session_setup(offer: &Offer) -> Result<Vec<u8>, Failure> {
    Ok(binary(dictionary([
        ("deviceID", device_id()?.into()),
        ("sessionUUID", random_uuid()?.into()),
        ("timingPort", u64::from(offer.timing_port).into()),
        ("timingProtocol", "NTP".into()),
    ])))
}

/// Claude: one realtime stream of uncompressed ALAC frames, with the fields
/// OwnTone and pyatv send. `latencyMin` is what receivers add to the sync
/// packets' mapping, by OwnTone's reading.
fn stream_setup(offer: &Offer, audio_key: &[u8; KEY_LEN], session_id: u32) -> Vec<u8> {
    let stream = dictionary([
        ("audioFormat", ALAC_44100_16_2.into()),
        ("audioMode", "default".into()),
        ("controlPort", u64::from(offer.control_port).into()),
        ("ct", COMPRESSION_ALAC.into()),
        ("isMedia", true.into()),
        ("latencyMax", LATENCY_MAX.into()),
        ("latencyMin", u64::from(ASSUMED_AUDIO_LATENCY).into()),
        ("shk", Value::Data(audio_key.to_vec())),
        ("spf", (FRAMES_PER_PACKET as u64).into()),
        ("sr", u64::from(SAMPLE_RATE).into()),
        ("type", REALTIME.into()),
        ("supportsDynamicStreamID", false.into()),
        ("streamConnectionID", u64::from(session_id).into()),
    ]);
    binary(dictionary([("streams", vec![stream].into())]))
}

/// Claude: a TEARDOWN naming the stream ends it and keeps the session, as iOS
/// sends first; one with an empty plist ends the session.
pub(crate) fn stream_teardown(stream: StreamRef) -> Vec<u8> {
    let StreamRef { kind, id } = stream;
    let mut stream = Dictionary::new();
    stream.insert("type".into(), kind.into());
    if let Some(id) = id {
        stream.insert("streamID".into(), id.into());
    }
    binary(dictionary([(
        "streams",
        vec![Value::Dictionary(stream)].into(),
    )]))
}

pub(crate) fn session_teardown() -> Vec<u8> {
    binary(Value::Dictionary(Dictionary::new()))
}

fn teardown<'a>(uri: &'a str, body: &'a [u8]) -> Request<'a> {
    Request {
        method: "TEARDOWN",
        uri,
        headers: vec![],
        body: Some((PLIST, body)),
    }
}

fn dictionary<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Dictionary(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

fn binary(value: Value) -> Vec<u8> {
    let mut out = Vec::new();
    value
        .to_writer_binary(&mut out)
        .expect("a plist of plain values writes to memory");
    out
}

/// Claude: a random device ID, as a MAC address reads.
fn device_id() -> Result<String, Failure> {
    let mut device = [0u8; 6];
    getrandom::fill(&mut device).map_err(|e| Failure::Failed(e.to_string()))?;
    Ok(device
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":"))
}

fn random_uuid() -> Result<String, Failure> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| Failure::Failed(e.to_string()))?;
    Ok(uuid_v4(bytes))
}

fn uuid_v4(mut bytes: [u8; 16]) -> String {
    bytes[6] = bytes[6] & 0x0F | 0x40;
    bytes[8] = bytes[8] & 0x3F | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

fn random_u32() -> Result<u32, Failure> {
    getrandom::u32().map_err(|e| Failure::Failed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(bytes: &[u8]) -> Dictionary {
        Value::from_reader(Cursor::new(bytes))
            .unwrap()
            .into_dictionary()
            .unwrap()
    }

    #[test]
    fn the_stream_setup_asks_for_realtime_alac_with_the_key_and_ports() {
        let offer = Offer {
            control_port: 6001,
            timing_port: 6002,
            first_seq: 0,
            first_rtp: 0,
        };
        let setup = read(&stream_setup(&offer, &[7; 32], 1234));
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
                "controlPort",
                "latencyMin",
                "streamConnectionID"
            ]
            .map(number),
            [96, 2, 0x40000, 352, 44_100, 6001, 11_025, 1234]
        );
        assert_eq!(stream["shk"].as_data(), Some(&[7; 32][..]));

        let session = read(&session_setup(&offer).unwrap());
        assert_eq!(session["timingProtocol"].as_string(), Some("NTP"));
        assert_eq!(session["timingPort"].as_unsigned_integer(), Some(6002));
        let device = session["deviceID"].as_string().unwrap();
        assert_eq!((device.len(), device.matches(':').count()), (17, 5));
        assert_eq!(session["sessionUUID"].as_string().unwrap().len(), 36);
        assert_eq!(session.len(), 4);
    }

    #[test]
    fn teardowns_name_the_stream_then_nothing() {
        let stream = read(&stream_teardown(StreamRef {
            kind: REALTIME,
            id: Some(5),
        }));
        let entry = stream["streams"].as_array().unwrap()[0]
            .as_dictionary()
            .unwrap();
        assert_eq!(entry["type"].as_unsigned_integer(), Some(96));
        assert_eq!(entry["streamID"].as_unsigned_integer(), Some(5));
        assert!(read(&session_teardown()).is_empty());
    }

    #[test]
    fn session_uuids_are_version_4() {
        let uuid = uuid_v4([0xFF; 16]);
        assert_eq!(uuid, "FFFFFFFF-FFFF-4FFF-BFFF-FFFFFFFFFFFF");
    }
}
