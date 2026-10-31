use std::sync::atomic::{AtomicU64, Ordering};

use spaghetti_common::ipc_protocol::{FlowState, FlowStatus};

use crate::host_time;

const STALL_AFTER_SECONDS: f64 = 3.0;

/// Claude: counts the media one end of a flow moves. Safe to record from a
/// real-time thread.
pub struct FlowMeter {
    bytes: AtomicU64,
    /// Claude: host time of the latest progress, 0 before any.
    last_at: AtomicU64,
}

/// Claude: what the end's transport says about itself, apart from media progress.
pub enum Link {
    Idle,
    Connecting,
    Connected {
        peer: Option<String>,
        packets_lost: u64,
        packets_retransmitted: u64,
    },
}

impl FlowMeter {
    pub const fn new() -> Self {
        Self {
            bytes: AtomicU64::new(0),
            last_at: AtomicU64::new(0),
        }
    }

    pub fn record(&self, bytes: u64, now: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        self.last_at.store(now.max(1), Ordering::Relaxed);
    }

    /// Claude: a new connection has not moved media yet, whatever the last one did.
    pub fn restart(&self) {
        self.last_at.store(0, Ordering::Relaxed);
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Claude: what a connected end's media did lately.
    pub fn progress(&self) -> FlowState {
        let last_at = self.last_at.load(Ordering::Relaxed);
        if last_at == 0 {
            return FlowState::Connected;
        }
        let now = host_time::now();
        let age = now.saturating_sub(last_at) as f64 / host_time::ticks_per_second();
        if age > STALL_AFTER_SECONDS {
            FlowState::Stalled
        } else {
            FlowState::Flowing
        }
    }

    pub fn status(&self, link: Link) -> FlowStatus {
        let (state, peer, packets_lost, packets_retransmitted) = match link {
            Link::Idle => (FlowState::Idle, None, 0, 0),
            Link::Connecting => (FlowState::Connecting, None, 0, 0),
            Link::Connected {
                peer,
                packets_lost,
                packets_retransmitted,
            } => (self.progress(), peer, packets_lost, packets_retransmitted),
        };
        FlowStatus {
            state,
            bytes: self.bytes(),
            peer,
            relayed: false,
            packets_lost,
            packets_retransmitted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected() -> Link {
        Link::Connected {
            peer: None,
            packets_lost: 0,
            packets_retransmitted: 0,
        }
    }

    #[test]
    fn a_connected_flow_is_flowing_while_media_moves_and_stalls_after() {
        let meter = FlowMeter::new();
        assert_eq!(meter.status(connected()).state, FlowState::Connected);
        let now = host_time::now();
        meter.record(8, now);
        assert_eq!(meter.status(connected()).state, FlowState::Flowing);
        assert_eq!(meter.status(connected()).bytes, 8);

        let long_ago = now - (4.0 * host_time::ticks_per_second()) as u64;
        meter.record(8, long_ago);
        assert_eq!(meter.status(connected()).state, FlowState::Stalled);
        meter.restart();
        assert_eq!(meter.status(connected()).state, FlowState::Connected);
        assert_eq!(meter.status(connected()).bytes, 16);
    }

    #[test]
    fn an_unconnected_flow_reports_its_link_whatever_moved() {
        let meter = FlowMeter::new();
        meter.record(8, host_time::now());
        assert_eq!(meter.status(Link::Idle).state, FlowState::Idle);
        assert_eq!(meter.status(Link::Connecting).state, FlowState::Connecting);
    }
}
