mod browse;
mod stream;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

pub use browse::Browser;
pub use stream::{Finishing, Stream};

pub const SAMPLE_RATE: u32 = 44_100;
pub const FRAMES_PER_PACKET: usize = 352;
/// Claude: how long before its play time a stream sends each packet.
pub const LATENCY: Duration = Duration::from_secs(2);

/// Claude: a receiver's AirPlay device ID, such as `C4:38:75:04:5E:80`, stable
/// across renames and restarts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReceiverId(String);

impl ReceiverId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for ReceiverId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        todo!()
    }
}

impl std::fmt::Display for ReceiverId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Claude: an AirPlay receiver found on the network, with each service it
/// advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receiver {
    pub id: ReceiverId,
    pub name: String,
    pub model: Option<String>,
    raop: Option<Service>,
    airplay: Option<Service>,
}

impl Receiver {
    /// Claude: whether a stream can play to it, or why not.
    pub fn support(&self) -> Result<(), String> {
        todo!()
    }
}

/// Claude: one advertised service: where it listens, as resolved on each
/// interface (IPv6 link-local addresses keep their scope), and its TXT record.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Service {
    addresses: Vec<SocketAddr>,
    txt: Vec<(String, String)>,
}

/// Claude: one packet a stream asks its renderer for: `FRAMES_PER_PACKET`
/// frames of stereo at `SAMPLE_RATE`, as left and right, that the receiver
/// plays from `play_at` on.
pub struct Render<'a> {
    pub channels: [&'a mut [f32]; 2],
    pub play_at: Instant,
}

/// Claude: the session's transport, not whether what it sends is audible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Streaming(Counters),
    /// Claude: the session never started. Final.
    Unreachable(String),
    /// Claude: the session ended after it started, for example because another
    /// sender took the receiver over. Final.
    Lost(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub packets_sent: u64,
    pub packets_retransmitted: u64,
    pub bytes_sent: u64,
}
