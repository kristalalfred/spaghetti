use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Claude: a PTP clock's identity. A receiver's grandmaster identity is the
/// timeline a buffered stream anchors to (SETRATEANCHORTIME's
/// `networkTimeTimelineID`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ClockIdentity(pub(crate) [u8; 8]);

impl ClockIdentity {
    pub(crate) fn as_u64(self) -> u64 {
        u64::from_be_bytes(self.0)
    }
}

/// Claude: which numbers a timebase's times are: the grandmaster's clock, and
/// an epoch that changes whenever that clock's time stops continuing what it
/// read before (a phase step under the same identity). A time from one
/// timeline means nothing on another; an anchor given on an older one must be
/// given again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Timeline {
    pub(crate) clock: ClockIdentity,
    pub(crate) epoch: u32,
}

/// Claude: a time on a PTP timeline, in nanoseconds since its epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PtpTime {
    pub(crate) nanos: u64,
}

impl PtpTime {
    /// Claude: whole seconds, as SETRATEANCHORTIME's `networkTimeSecs`.
    pub(crate) fn secs(self) -> u64 {
        self.nanos / NANOS_PER_SECOND
    }

    /// Claude: the fraction of a second in units of 2^-64 s, as
    /// SETRATEANCHORTIME's `networkTimeFrac`.
    pub(crate) fn frac(self) -> u64 {
        ((u128::from(self.nanos % NANOS_PER_SECOND) << 64) / u128::from(NANOS_PER_SECOND)) as u64
    }

    pub(crate) fn after(self, duration: Duration) -> PtpTime {
        PtpTime {
            nanos: self.nanos + duration.as_nanos() as u64,
        }
    }
}

/// Claude: a receiver's timeline as known at one moment: which timeline it is
/// and how host time maps onto it. Copied out of a `Peer` and used without
/// locking; read a fresh one as the estimate improves.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Timebase {
    timeline: Timeline,
    host: Instant,
    network: PtpTime,
    rate: f64,
}

impl Timebase {
    /// Claude: the timeline reads `network` at host instant `host` and
    /// advances `rate` seconds per host second.
    pub(crate) fn new(timeline: Timeline, host: Instant, network: PtpTime, rate: f64) -> Self {
        Timebase {
            timeline,
            host,
            network,
            rate,
        }
    }

    pub(crate) fn timeline(&self) -> Timeline {
        self.timeline
    }

    /// Claude: the timeline's time at host instant `at`.
    pub(crate) fn network(&self, at: Instant) -> PtpTime {
        let host_nanos = signed_nanos(at, self.host);
        let nanos = self.network.nanos as i128 + (host_nanos as f64 * self.rate).round() as i128;
        PtpTime {
            nanos: nanos.max(0) as u64,
        }
    }

    /// Claude: the host instant at which the timeline reads `at`.
    pub(crate) fn host(&self, at: PtpTime) -> Instant {
        let network_nanos = at.nanos as i128 - self.network.nanos as i128;
        let host_nanos = (network_nanos as f64 / self.rate).round() as i128;
        let offset = Duration::from_nanos(host_nanos.unsigned_abs() as u64);
        if host_nanos >= 0 {
            self.host + offset
        } else {
            self.host - offset
        }
    }
}

fn signed_nanos(at: Instant, from: Instant) -> i128 {
    match at.checked_duration_since(from) {
        Some(after) => after.as_nanos() as i128,
        None => -((from - at).as_nanos() as i128),
    }
}

/// Claude: where a peer takes PTP's event (Sync, Delay_Req) and general
/// (Announce, Follow_Up, Delay_Resp, Signaling) messages. Addresses keep their
/// IPv6 scope, since receivers are often reached over link-local addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Endpoint {
    pub(crate) event: SocketAddr,
    pub(crate) general: SocketAddr,
}

impl Endpoint {
    /// Claude: PTP's own ports, 319 and 320, at `address`'s IP and scope.
    pub(crate) fn standard(address: SocketAddr) -> Endpoint {
        let with_port = |port| {
            let mut address = address;
            address.set_port(port);
            address
        };
        Endpoint {
            event: with_port(319),
            general: with_port(320),
        }
    }
}

/// Claude: how a clock binds and announces itself.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    /// Claude: 0 binds an ephemeral port.
    pub(crate) event_port: u16,
    pub(crate) general_port: u16,
    pub(crate) priority1: u8,
}

impl Config {
    /// Claude: PTP's own ports and the priority a Mac announces, which an
    /// AirPlay receiver able to lead outranks.
    pub(crate) fn standard() -> Config {
        Config {
            event_port: 319,
            general_port: 320,
            priority1: 250,
        }
    }
}

/// Claude: this host's PTP clock. One per process holds UDP 319 and 320 for
/// as long as any stream holds it.
pub(crate) struct Ptp {}

impl Ptp {
    /// Claude: the process's clock on PTP's own ports, bound by the first
    /// caller and released when the last `Arc` drops, or why the ports could
    /// not be had.
    pub(crate) fn shared() -> Result<Arc<Ptp>, String> {
        todo!()
    }

    /// Claude: a clock of its own, outside the process's shared one, as tests
    /// and the fake receiver run.
    pub(crate) fn bind(config: Config) -> Result<Arc<Ptp>, String> {
        todo!()
    }

    /// Claude: where peers reach this clock at `local`'s IP and scope.
    pub(crate) fn endpoint(&self, local: SocketAddr) -> Endpoint {
        todo!()
    }

    pub(crate) fn identity(&self) -> ClockIdentity {
        todo!()
    }

    /// Claude: starts timing with the receiver at `peer`: following its clock
    /// when it wins the best master comparison against this clock, leading it
    /// otherwise. Dropping the `Peer` stops.
    pub(crate) fn add_peer(self: &Arc<Self>, peer: Endpoint) -> Peer {
        todo!()
    }
}

/// Claude: where timing with one receiver stands.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Timing {
    /// Claude: not yet led, or not yet followed closely enough to schedule
    /// against.
    Acquiring,
    /// Claude: the receiver's timeline, also while following through a short
    /// silence of its master (holdover).
    Settled(Timebase),
    /// Claude: the followed master went silent for longer than holdover lasts.
    /// Final for this peer.
    Lost(String),
}

/// Claude: one receiver this clock times with.
pub(crate) struct Peer {}

impl Peer {
    pub(crate) fn timing(&self) -> Timing {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMELINE: Timeline = Timeline {
        clock: ClockIdentity([1, 2, 3, 4, 5, 6, 7, 8]),
        epoch: 0,
    };

    #[test]
    fn anchor_times_split_into_seconds_and_a_binary_fraction() {
        let time = PtpTime {
            nanos: 231_017_856_645_460,
        };
        assert_eq!(time.secs(), 231_017);
        let nanos_back = (u128::from(time.frac()) * 1_000_000_000).div_ceil(1 << 64);
        assert_eq!(nanos_back, 856_645_460);
        assert_eq!(PtpTime { nanos: 500_000_000 }.frac(), 1 << 63);
        assert_eq!(PtpTime { nanos: 250_000_000 }.frac(), 1 << 62);
        assert_eq!(PtpTime { nanos: 7_000_000_000 }.frac(), 0);
    }

    #[test]
    fn a_timebase_maps_host_instants_both_ways_at_its_rate() {
        let host = Instant::now() + Duration::from_secs(100);
        let network = PtpTime {
            nanos: 231_017_000_000_000,
        };
        let timebase = Timebase::new(TIMELINE, host, network, 1.0001);
        let later = host + Duration::from_secs(10);
        assert_eq!(timebase.network(later).nanos, network.nanos + 10_001_000_000);
        let earlier = host - Duration::from_secs(10);
        assert_eq!(timebase.network(earlier).nanos, network.nanos - 10_001_000_000);
        for at in [later, earlier, host] {
            let back = timebase.host(timebase.network(at));
            assert!(signed_nanos(back, at).abs() <= 1);
        }
    }

    #[test]
    fn standard_endpoints_keep_the_address_and_its_scope() {
        let scoped: SocketAddr = "[fe80::d85c:c0f5:c1a0:d348%1]:7000".parse().unwrap();
        let endpoint = Endpoint::standard(scoped);
        assert_eq!(endpoint.event.to_string(), "[fe80::d85c:c0f5:c1a0:d348%1]:319");
        assert_eq!(endpoint.general.to_string(), "[fe80::d85c:c0f5:c1a0:d348%1]:320");
    }
}
