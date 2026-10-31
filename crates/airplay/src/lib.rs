mod backlog;
mod browse;
mod clock;
mod dns_sd;
#[cfg(test)]
mod fake;
mod hap;
mod packet;
mod raop;
mod receiver;
mod rtsp;
mod srp;
mod stream;
mod tlv8;
mod turns;

use std::time::{Duration, Instant};

pub use browse::Browser;
pub use receiver::{Protocol, Receiver, ReceiverId};
pub use stream::{Finishing, Stream};

pub const SAMPLE_RATE: u32 = 44_100;
pub const FRAMES_PER_PACKET: usize = 352;
/// Claude: how long before its play time a stream sends each packet.
pub const LATENCY: Duration = Duration::from_secs(2);

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
