use std::io::Cursor;
use std::net::SocketAddr;
use std::time::Duration;

use plist::{Dictionary, Value};
use tracing::{debug, warn};

use crate::events::EventChannel;
use crate::hap::{self, Counted, KEY_LEN};
use crate::pairing;
use crate::rtsp::{Connection, PLIST, Request, Response};
use crate::session::{self, ASSUMED_AUDIO_LATENCY, Offer, Session, Transport, refusal};
use crate::{FRAMES_PER_PACKET, SAMPLE_RATE};

/// Claude: how often iOS, and so pyatv, posts `/feedback` to keep a session.
pub(crate) const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);
/// Claude: the User-Agent OwnTone sends, whose AirPlay 2 sessions are this one:
/// transient pairing, NTP timing, realtime ALAC.
pub(crate) const USER_AGENT: &str = "AirPlay/381.13";
pub(crate) const REALTIME: u64 = 96;
/// Claude: ALAC, 44.1 kHz, 16-bit stereo, in AirPlay 2's audio format bits.
const ALAC_44100_16_2: u64 = 0x40000;
const COMPRESSION_ALAC: u64 = 2;
const LATENCY_MAX: u64 = 88_200;

/// Claude: why an AirPlay 2 handshake did not end in a session. A receiver
/// that refused a step may still take AirPlay 1; one that failed, by not
/// answering, answering malformed or proving the wrong key, is not tried
/// again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    Refused(String),
    Failed(String),
}

impl Failure {
    pub(crate) fn reason(&self) -> &str {
        match self {
            Failure::Refused(reason) | Failure::Failed(reason) => reason,
        }
    }
}

/// Claude: asks for the receiver's `/info`, as OwnTone and pyatv do first,
/// pairs transiently, encrypts the connection, then sets up the session with
/// NTP timing, opens the receiver's event connection, sets up one realtime
/// ALAC stream with a fresh key, and records. A handshake that fails after
/// the session's SETUP tears the session down within `teardown_timeout`.
pub(crate) fn negotiate(
    mut connection: Connection,
    offer: &Offer,
    timeout: Duration,
    teardown_timeout: Duration,
    abort: &dyn Fn() -> bool,
) -> Result<Session, Failure> {
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
    let reply = setup(&mut connection, &uri, session_setup(offer)?, timeout, abort)?;
    let event_port = port(&reply, "eventPort")?;
    let mut events_at = connection.peer_addr();
    events_at.set_port(event_port);
    let events_keys = hap::keys(
        &secret,
        "Events-Salt",
        "Events-Read-Encryption-Key",
        "Events-Write-Encryption-Key",
    );
    let events = match EventChannel::connect(events_at, &events_keys, timeout, abort) {
        Ok(events) => Some(events),
        Err(reason) if abort() => return Err(Failure::Failed(reason)),
        Err(reason) => {
            warn!(%reason, "AirPlay 2 receiver's event connection failed; streaming without it");
            None
        }
    };
    let mut audio_key = [0; KEY_LEN];
    getrandom::fill(&mut audio_key).map_err(|e| Failure::Failed(e.to_string()))?;
    let stream = stream_setup(offer, &audio_key, session_id);
    match record(&mut connection, &uri, stream, timeout, abort) {
        Ok((audio, control, stream_id, record)) => Ok(Session {
            audio: with_port(connection.peer_addr(), audio),
            control: with_port(connection.peer_addr(), control),
            audio_latency: session::audio_latency(&record),
            connection,
            uri,
            transport: Transport::AirPlay2 {
                audio_key: Counted::new(&audio_key),
                _events: events,
                stream_id,
            },
        }),
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

/// Claude: the stream's SETUP and the RECORD after it: the receiver's audio
/// and control ports, its stream ID if it gave one, and the RECORD answer.
fn record(
    connection: &mut Connection,
    uri: &str,
    stream: Vec<u8>,
    timeout: Duration,
    abort: &dyn Fn() -> bool,
) -> Result<(u16, u16, Option<u64>, Response), Failure> {
    let reply = setup(connection, uri, stream, timeout, abort)?;
    let stream = reply
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|streams| streams.first())
        .and_then(Value::as_dictionary)
        .ok_or_else(|| Failure::Failed("its stream SETUP answer has no stream".into()))?;
    let (audio, control) = (port(stream, "dataPort")?, port(stream, "controlPort")?);
    let stream_id = stream.get("streamID").and_then(Value::as_unsigned_integer);
    let response = exchange(
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
    Ok((audio, control, stream_id, response))
}

fn setup(
    connection: &mut Connection,
    uri: &str,
    body: Vec<u8>,
    timeout: Duration,
    abort: &dyn Fn() -> bool,
) -> Result<Dictionary, Failure> {
    let request = Request {
        method: "SETUP",
        uri,
        headers: vec![],
        body: Some((PLIST, &body)),
    };
    let response = exchange(connection, &request, timeout, abort)?;
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
        return Err(Failure::Refused(refusal(request.method, &response)));
    }
    Ok(response)
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

/// Claude: the session's SETUP: the fields OwnTone sends for NTP timing, and
/// those of pyatv's that describe the session rather than claim to be an
/// iPhone, with a group of its own as OwnTone gives each session.
fn session_setup(offer: &Offer) -> Result<Vec<u8>, Failure> {
    let mut device = [0u8; 6];
    let mut session = [0u8; 16];
    let mut group = [0u8; 16];
    getrandom::fill(&mut device)
        .and_then(|()| getrandom::fill(&mut session))
        .and_then(|()| getrandom::fill(&mut group))
        .map_err(|e| Failure::Failed(e.to_string()))?;
    let device_id = device
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":");
    Ok(binary(dictionary([
        ("deviceID", device_id.clone().into()),
        ("macAddress", device_id.into()),
        ("sessionUUID", uuid_v4(session).into()),
        ("groupUUID", uuid_v4(group).into()),
        ("groupContainsGroupLeader", false.into()),
        ("isMultiSelectAirPlay", true.into()),
        ("senderSupportsRelay", false.into()),
        ("statsCollectionEnabled", false.into()),
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
pub(crate) fn stream_teardown(stream_id: Option<u64>) -> Vec<u8> {
    let mut stream = Dictionary::new();
    stream.insert("type".into(), REALTIME.into());
    if let Some(id) = stream_id {
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
        assert!(session["sessionUUID"] != session["groupUUID"]);
        assert_eq!(
            session["groupContainsGroupLeader"].as_boolean(),
            Some(false)
        );
        assert!(!session.contains_key("model") && !session.contains_key("osName"));
    }

    #[test]
    fn teardowns_name_the_stream_then_nothing() {
        let stream = read(&stream_teardown(Some(5)));
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
