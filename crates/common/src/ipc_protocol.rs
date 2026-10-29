use serde::{Deserialize, Serialize};
use std::io::{Read, Result as IoResult, Write};

/// How a tap affects its source process's audio output.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum TapMode {
    /// Source continues playing; the tap is a parallel copy.
    #[default]
    Mirror,
    /// Source output is silenced while the tap is active; audio only flows via the route.
    MuteSourceWhenTapped,
}

impl TapMode {
    pub fn label(self) -> &'static str {
        match self {
            TapMode::Mirror => "mirror",
            TapMode::MuteSourceWhenTapped => "muted",
        }
    }
}

/// Claude: changes whenever a request or response changes shape, so a client
/// can refuse a daemon it does not speak.
pub const IPC_VERSION: u32 = 4;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Status,
    ListApps,
    /// `tag` is the client's own name for the source, listed back with it.
    CreateSource { source: SourceSpec, tag: Option<String> },
    DestroySource { source_id: u32 },
    /// Replace the processes a tap source mixes, keeping its routes running.
    SetTapProcesses { source_id: u32, pids: Vec<u32> },
    ListSources,
    CreateRoute { source_id: u32, destination: Destination, volume: f32 },
    DestroyRoute { route_id: u32 },
    ListRoutes,
    SetVolume { route_id: u32, volume: f32 },
    /// Hand a WebRTC peer the answer to its offer, or the offer it answers.
    SetRemoteDescription { peer: PeerRef, generation: u32, sdp: String },
    CreateDevice { name: String, channels: Option<u32> },
    DestroyDevice { device_uid: String },
    ListDevices,
    GetActivity,
    ListSystemDevices,
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Error { message: String },
    Status(DaemonStatus),
    Apps(Vec<AppInfo>),
    Sources(Vec<SourceInfo>),
    Routes(Vec<RouteInfo>),
    SourceCreated { source_id: u32 },
    RouteCreated { route_id: u32 },
    DeviceCreated { device_uid: String },
    Devices(Vec<DeviceInfo>),
    Activity(ActivityInfo),
    SystemDevices(Vec<SystemDeviceInfo>),
}

/// Where a route's audio comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceSpec {
    /// The audio of these processes, mixed to stereo.
    Tap { pids: Vec<u32>, mode: TapMode },
    /// AAC in MPEG-TS received over SRT.
    Srt(SrtSocket),
    /// Opus received from one WebRTC peer, decoded to 48 kHz stereo.
    WebRtc(WebRtcPeer),
}

/// Where a route sends its source's audio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Destination {
    /// A Core Audio output device, by UID. With `delay_ms`, each frame sounds
    /// that long after its source captured it, so routes with one delay play
    /// in step, on this Mac and on Macs receiving the source over WebRTC.
    /// Taps and WebRTC sources date their frames; SRT sources do not.
    Device {
        uid: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delay_ms: Option<u32>,
    },
    /// AAC in MPEG-TS sent over SRT.
    Srt(SrtSocket),
    /// Opus sent to one WebRTC peer: 48 kHz stereo in 20 ms packets.
    WebRtc(WebRtcPeer),
}

/// One end of a WebRTC peer link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebRtcPeer {
    /// The controller's id for the link, opaque to spaghetti.
    pub link: String,
    /// Bumped by the controller whenever the link must start over.
    pub generation: u32,
    pub role: PeerRole,
    /// STUN servers as host:port; empty for host candidates only.
    pub stun: Vec<String>,
    /// TURN servers, each adding a relayed candidate; empty for none.
    pub turn: Vec<TurnServer>,
}

/// A UDP TURN server and the credentials to allocate a relay on it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnServer {
    /// host:port
    pub address: String,
    pub username: String,
    pub credential: String,
}

impl std::fmt::Debug for TurnServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnServer")
            .field("address", &self.address)
            .field("username", &self.username)
            .field("credential", &"<redacted>")
            .finish()
    }
}

/// The side of the offer/answer exchange a peer takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerRole {
    Offer,
    Answer,
}

/// A WebRTC peer, by the source or route it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerRef {
    Source(u32),
    Route(u32),
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SrtSocket {
    pub mode: SrtMode,
    pub latency_ms: u32,
    pub passphrase: Option<String>,
    /// AES key length in bytes: 16, 24 or 32. Only used with a passphrase.
    pub key_size: Option<u8>,
}

impl std::fmt::Debug for SrtSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SrtSocket")
            .field("mode", &self.mode)
            .field("latency_ms", &self.latency_ms)
            .field("passphrase", &self.passphrase.as_ref().map(|_| "<redacted>"))
            .field("key_size", &self.key_size)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SrtMode {
    /// Wait for a caller on this UDP port on every interface.
    Listen { port: u16 },
    /// Dial a listener.
    Call { host: String, port: u16 },
}

/// How media moves through one end of a source or route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlowState {
    /// Not running, for example a tap no route reads from.
    Idle,
    /// Waiting for a peer or dialing one.
    Connecting,
    /// Running, but no media has moved yet.
    Connected,
    /// Media moved within the last few seconds.
    Flowing,
    /// Media moved before but not within the last few seconds.
    Stalled,
    /// The peer was never reached. Final until the spec changes.
    Unreachable,
    /// The connection to the peer ended. Final until the spec changes.
    Lost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowStatus {
    pub state: FlowState,
    /// Payload bytes moved so far: encoded bytes for SRT and WebRTC, PCM bytes otherwise.
    pub bytes: u64,
    /// The connected SRT or WebRTC peer.
    pub peer: Option<String>,
    /// Claude: most of a connected WebRTC flow's recent datagrams came through
    /// a TURN relay: this end's, or the remote's at one of this link's servers.
    pub relayed: bool,
    pub packets_lost: u64,
    pub packets_retransmitted: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub running: bool,
    pub ipc_version: u32,
    pub source_count: u32,
    pub route_count: u32,
}

/// An app and the processes of it Core Audio knows.
#[derive(Debug, Serialize, Deserialize)]
pub struct AppInfo {
    pub name: String,
    pub bundle_id: Option<String>,
    /// The `.app` the processes run from; absent for a process outside any bundle.
    pub bundle_path: Option<String>,
    pub pids: Vec<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SourceInfo {
    pub source_id: u32,
    pub spec: SourceSpec,
    pub tag: Option<String>,
    /// The tapped app's name, the SRT endpoint, or the WebRTC link.
    pub label: String,
    pub status: FlowStatus,
    /// A WebRTC peer's offer or answer, once it has one.
    pub local_description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RouteInfo {
    pub route_id: u32,
    pub source_id: u32,
    pub destination: Destination,
    pub volume: f32,
    pub status: FlowStatus,
    /// A WebRTC peer's offer or answer, once it has one.
    pub local_description: Option<String>,
}

/// Snapshot of driver-side audio activity, derived from the shared-memory region.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityInfo {
    /// True when IO is running AND the driver wrote a block within the freshness window.
    pub active: bool,
    /// Frames-per-second over a ~500ms rolling window.
    pub frames_per_second: u64,
    /// Peak amplitude of the last block in dBFS. Floored at -96.0.
    pub peak_db: f32,
    /// Raw `io_running` flag from the driver.
    pub io_running: bool,
    /// Total frames the driver has ever written.
    pub total_frames: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub uid: String,
    pub name: String,
    pub channels: u32,
    /// True for the built-in Spaghetti device that cannot be destroyed.
    #[serde(default)]
    pub is_default: bool,
}

/// A CoreAudio device as seen by the system — not necessarily daemon-managed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemDeviceInfo {
    pub uid: String,
    pub name: String,
    /// Output channel count (0 for input-only devices).
    pub channels: u32,
    /// False when the device has no output streams.
    pub is_output: bool,
}

/// Socket path for daemon IPC.
pub const SOCKET_PATH: &str = "/tmp/spaghetti.sock";

/// Write a length-prefixed JSON message.
pub fn write_message<W: Write>(writer: &mut W, msg: &impl Serialize) -> IoResult<()> {
    let json = serde_json::to_vec(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let len = (json.len() as u32).to_be_bytes();
    writer.write_all(&len)?;
    writer.write_all(&json)?;
    writer.flush()
}

/// Read a length-prefixed JSON message.
pub fn read_message<R: Read, T: serde::de::DeserializeOwned>(reader: &mut R) -> IoResult<T> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude: `value` serializes to exactly `json`, and `json` reads back to it.
    fn golden<T: Serialize + serde::de::DeserializeOwned>(value: &T, json: &str) {
        assert_eq!(serde_json::to_string(value).unwrap(), json);
        let read: T = serde_json::from_str(json).unwrap();
        assert_eq!(serde_json::to_string(&read).unwrap(), json);
    }

    fn answering() -> WebRtcPeer {
        WebRtcPeer {
            link: "link-7".into(),
            generation: 2,
            role: PeerRole::Answer,
            stun: vec!["stun.l.google.com:19302".into(), "203.0.113.9:3478".into()],
            turn: vec![TurnServer {
                address: "turn.example.net:3478".into(),
                username: "1767225600:link-7".into(),
                credential: "c2VjcmV0".into(),
            }],
        }
    }

    fn offering() -> WebRtcPeer {
        WebRtcPeer {
            link: "link-7".into(),
            generation: 3,
            role: PeerRole::Offer,
            stun: Vec::new(),
            turn: Vec::new(),
        }
    }

    const ANSWERING: &str = r#"{"link":"link-7","generation":2,"role":"Answer","stun":["stun.l.google.com:19302","203.0.113.9:3478"],"turn":[{"address":"turn.example.net:3478","username":"1767225600:link-7","credential":"c2VjcmV0"}]}"#;
    const OFFERING: &str = r#"{"link":"link-7","generation":3,"role":"Offer","stun":[],"turn":[]}"#;

    fn connecting() -> FlowStatus {
        FlowStatus {
            state: FlowState::Connecting,
            bytes: 0,
            peer: None,
            relayed: false,
            packets_lost: 0,
            packets_retransmitted: 0,
        }
    }

    #[test]
    fn a_device_destination_carries_its_delay_only_when_it_has_one() {
        golden(
            &Destination::Device {
                uid: "Speakers".into(),
                delay_ms: None,
            },
            r#"{"Device":{"uid":"Speakers"}}"#,
        );
        golden(
            &Destination::Device {
                uid: "Speakers".into(),
                delay_ms: Some(300),
            },
            r#"{"Device":{"uid":"Speakers","delay_ms":300}}"#,
        );
    }

    #[test]
    fn webrtc_specs_have_a_fixed_wire_shape() {
        golden(&answering(), ANSWERING);
        golden(&offering(), OFFERING);
        golden(&SourceSpec::WebRtc(answering()), &format!(r#"{{"WebRtc":{ANSWERING}}}"#));
        golden(&Destination::WebRtc(offering()), &format!(r#"{{"WebRtc":{OFFERING}}}"#));
        golden(
            &answering().turn[0],
            r#"{"address":"turn.example.net:3478","username":"1767225600:link-7","credential":"c2VjcmV0"}"#,
        );
        golden(&PeerRole::Offer, r#""Offer""#);
        golden(&PeerRole::Answer, r#""Answer""#);
        golden(&PeerRef::Source(4), r#"{"Source":4}"#);
        golden(&PeerRef::Route(9), r#"{"Route":9}"#);
        golden(&FlowState::Unreachable, r#""Unreachable""#);
        golden(&FlowState::Lost, r#""Lost""#);
    }

    #[test]
    fn a_turn_credential_stays_out_of_debug_output() {
        let debug = format!("{:?}", answering());
        assert!(debug.contains("turn.example.net:3478"), "{debug}");
        assert!(debug.contains("1767225600:link-7"), "{debug}");
        assert!(!debug.contains("c2VjcmV0"), "{debug}");
    }

    #[test]
    fn webrtc_requests_have_a_fixed_wire_shape() {
        golden(
            &Request::SetRemoteDescription {
                peer: PeerRef::Route(9),
                generation: 3,
                sdp: "v=0\r\no=- 1 2 IN IP4 0.0.0.0\r\n".into(),
            },
            r#"{"SetRemoteDescription":{"peer":{"Route":9},"generation":3,"sdp":"v=0\r\no=- 1 2 IN IP4 0.0.0.0\r\n"}}"#,
        );
        golden(
            &Request::CreateSource {
                source: SourceSpec::WebRtc(answering()),
                tag: Some("mic".into()),
            },
            &format!(r#"{{"CreateSource":{{"source":{{"WebRtc":{ANSWERING}}},"tag":"mic"}}}}"#),
        );
        golden(
            &Request::CreateRoute {
                source_id: 4,
                destination: Destination::WebRtc(offering()),
                volume: 1.0,
            },
            &format!(r#"{{"CreateRoute":{{"source_id":4,"destination":{{"WebRtc":{OFFERING}}},"volume":1.0}}}}"#),
        );
    }

    #[test]
    fn listings_carry_the_local_description() {
        golden(
            &SourceInfo {
                source_id: 4,
                spec: SourceSpec::WebRtc(answering()),
                tag: None,
                label: "link-7".into(),
                status: connecting(),
                local_description: Some("v=0\r\n".into()),
            },
            &format!(
                r#"{{"source_id":4,"spec":{{"WebRtc":{ANSWERING}}},"tag":null,"label":"link-7","status":{{"state":"Connecting","bytes":0,"peer":null,"relayed":false,"packets_lost":0,"packets_retransmitted":0}},"local_description":"v=0\r\n"}}"#
            ),
        );
        golden(
            &RouteInfo {
                route_id: 9,
                source_id: 4,
                destination: Destination::WebRtc(offering()),
                volume: 0.5,
                status: FlowStatus {
                    state: FlowState::Flowing,
                    bytes: 1200,
                    peer: Some("203.0.113.1:3478".into()),
                    relayed: true,
                    packets_lost: 3,
                    packets_retransmitted: 0,
                },
                local_description: None,
            },
            &format!(
                r#"{{"route_id":9,"source_id":4,"destination":{{"WebRtc":{OFFERING}}},"volume":0.5,"status":{{"state":"Flowing","bytes":1200,"peer":"203.0.113.1:3478","relayed":true,"packets_lost":3,"packets_retransmitted":0}},"local_description":null}}"#
            ),
        );
    }
}
