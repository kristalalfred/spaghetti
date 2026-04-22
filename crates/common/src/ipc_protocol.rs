use serde::{Deserialize, Serialize};
use std::io::{Read, Result as IoResult, Write};

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Status,
    ListApps,
    CreateTap { pid: u32 },
    DestroyTap { tap_id: u32 },
    ListTaps,
    CreateRoute { tap_id: u32, device_uid: String, volume: f32 },
    DestroyRoute { route_id: u32 },
    ListRoutes,
    SetVolume { route_id: u32, volume: f32 },
    CreateDevice { name: String, channels: Option<u32> },
    DestroyDevice { device_uid: String },
    ListDevices,
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Error { message: String },
    Status(DaemonStatus),
    Apps(Vec<AppInfo>),
    Taps(Vec<TapInfo>),
    Routes(Vec<RouteInfo>),
    TapCreated { tap_id: u32 },
    RouteCreated { route_id: u32 },
    DeviceCreated { device_uid: String },
    Devices(Vec<DeviceInfo>),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub running: bool,
    pub tap_count: u32,
    pub route_count: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AppInfo {
    pub pid: u32,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TapInfo {
    pub tap_id: u32,
    pub pid: u32,
    pub app_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RouteInfo {
    pub route_id: u32,
    pub tap_id: u32,
    pub device_uid: String,
    pub volume: f32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub uid: String,
    pub name: String,
    pub channels: u32,
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
