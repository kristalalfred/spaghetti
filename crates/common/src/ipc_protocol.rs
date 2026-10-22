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
pub const IPC_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Status,
    ListApps,
    CreateSource { source: SourceSpec },
    DestroySource { source_id: u32 },
    ListSources,
    CreateRoute { source_id: u32, destination: Destination, volume: f32 },
    DestroyRoute { route_id: u32 },
    ListRoutes,
    SetVolume { route_id: u32, volume: f32 },
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
}

/// Where a route sends its source's audio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Destination {
    /// A Core Audio output device, by UID.
    Device { uid: String },
    /// AAC in MPEG-TS sent over SRT.
    Srt(SrtSocket),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SrtSocket {
    pub mode: SrtMode,
    pub latency_ms: u32,
    pub passphrase: Option<String>,
    /// AES key length in bytes: 16, 24 or 32. Only used with a passphrase.
    pub key_size: Option<u8>,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowStatus {
    pub state: FlowState,
    /// Payload bytes moved so far: encoded bytes for SRT, PCM bytes otherwise.
    pub bytes: u64,
    /// The connected SRT peer.
    pub peer: Option<String>,
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

#[derive(Debug, Serialize, Deserialize)]
pub struct AppInfo {
    pub pid: u32,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SourceInfo {
    pub source_id: u32,
    pub spec: SourceSpec,
    /// The tapped app's name, or the SRT endpoint.
    pub label: String,
    pub status: FlowStatus,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RouteInfo {
    pub route_id: u32,
    pub source_id: u32,
    pub destination: Destination,
    pub volume: f32,
    pub status: FlowStatus,
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
