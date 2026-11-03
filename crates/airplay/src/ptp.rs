mod datagram;
mod runtime;
mod servo;
mod wire;
use std::net::SocketAddr;
use std::os::unix::net::UnixDatagram;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak, mpsc};
use std::thread::{self, JoinHandle};
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

/// Codex: identifies the grandmaster and its phase epoch. A stream ends if
/// its anchored timeline changes.
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

    pub(crate) fn after(self, duration: Duration) -> Option<PtpTime> {
        Some(PtpTime {
            nanos: self
                .nanos
                .checked_add(u64::try_from(duration.as_nanos()).ok()?)?,
        })
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
    pub(crate) fn new(
        timeline: Timeline,
        host: Instant,
        network: PtpTime,
        rate: f64,
    ) -> Option<Self> {
        if !rate.is_finite() || rate <= 0.0 {
            return None;
        }
        Some(Timebase {
            timeline,
            host,
            network,
            rate,
        })
    }

    pub(crate) fn timeline(&self) -> Timeline {
        self.timeline
    }

    /// Claude: the timeline's time at host instant `at`.
    pub(crate) fn network(&self, at: Instant) -> Option<PtpTime> {
        let delta = (signed_nanos(at, self.host) as f64 * self.rate).round();
        let delta = checked_nanos(delta)?;
        let nanos = i128::from(self.network.nanos).checked_add(delta)?;
        Some(PtpTime {
            nanos: u64::try_from(nanos).ok()?,
        })
    }

    /// Claude: the host instant at which the timeline reads `at`.
    pub(crate) fn host(&self, at: PtpTime) -> Option<Instant> {
        let delta = (i128::from(at.nanos) - i128::from(self.network.nanos)) as f64 / self.rate;
        let nanos = checked_nanos(delta.round())?;
        let magnitude = nanos.unsigned_abs();
        let offset = Duration::new(
            u64::try_from(magnitude / u128::from(NANOS_PER_SECOND)).ok()?,
            (magnitude % u128::from(NANOS_PER_SECOND)) as u32,
        );
        if nanos >= 0 {
            self.host.checked_add(offset)
        } else {
            self.host.checked_sub(offset)
        }
    }
}

fn checked_nanos(nanos: f64) -> Option<i128> {
    if !nanos.is_finite() || !(i128::MIN as f64..i128::MAX as f64).contains(&nanos) {
        return None;
    }
    Some(nanos as i128)
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
pub(crate) struct Ptp {
    identity: ClockIdentity,
    event_port: u16,
    general_port: u16,
    control: runtime::Control,
    thread: Option<JoinHandle<()>>,
    next_peer: AtomicU64,
    shared_registry: Option<&'static Registry>,
}

struct Registry {
    clock: Mutex<Option<Weak<Ptp>>>,
    released: Condvar,
}

impl Registry {
    const fn new() -> Self {
        Self {
            clock: Mutex::new(None),
            released: Condvar::new(),
        }
    }

    fn acquire(&'static self, config: Config) -> Result<Arc<Ptp>, String> {
        let mut slot = self.clock.lock().unwrap();
        loop {
            match slot.as_ref() {
                Some(weak) => {
                    if let Some(clock) = weak.upgrade() {
                        return Ok(clock);
                    }
                    slot = self.released.wait(slot).unwrap();
                }
                None => {
                    let mut clock = Ptp::bind(config)?;
                    Arc::get_mut(&mut clock).unwrap().shared_registry = Some(self);
                    *slot = Some(Arc::downgrade(&clock));
                    return Ok(clock);
                }
            }
        }
    }
}

impl Drop for Ptp {
    fn drop(&mut self) {
        self.control.send(runtime::Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(registry) = self.shared_registry {
            *registry.clock.lock().unwrap() = None;
            registry.released.notify_all();
        }
    }
}

impl Ptp {
    /// Claude: the process's clock on PTP's own ports, bound by the first
    /// caller and released when the last `Arc` drops, or why the ports could
    /// not be had.
    pub(crate) fn shared() -> Result<Arc<Ptp>, String> {
        static SHARED: Registry = Registry::new();
        SHARED.acquire(Config::standard())
    }

    /// Claude: a clock of its own, outside the process's shared one, as tests
    /// and the fake receiver run.
    pub(crate) fn bind(config: Config) -> Result<Arc<Ptp>, String> {
        let mut bytes = [0; 8];
        getrandom::fill(&mut bytes).map_err(|e| format!("could not create PTP identity: {e}"))?;
        let identity = ClockIdentity(bytes);
        let sockets = runtime::Sockets::bind(&config)?;
        let event_port = sockets.event_port;
        let general_port = sockets.general_port;
        let (wake, reader) =
            UnixDatagram::pair().map_err(|e| format!("could not open PTP wake socket: {e}"))?;
        wake.set_nonblocking(true).map_err(|e| e.to_string())?;
        reader.set_nonblocking(true).map_err(|e| e.to_string())?;
        let (sender, commands) = mpsc::channel();
        let own = Timebase::new(
            Timeline {
                clock: identity,
                epoch: 0,
            },
            Instant::now(),
            PtpTime {
                nanos: NANOS_PER_SECOND,
            },
            1.0,
        )
        .unwrap();
        let thread = thread::Builder::new()
            .name("airplay-ptp".into())
            .spawn(move || runtime::run(sockets, reader, commands, own, config))
            .map_err(|e| format!("could not start PTP thread: {e}"))?;
        Ok(Arc::new(Self {
            identity,
            event_port,
            general_port,
            control: runtime::Control { sender, wake },
            thread: Some(thread),
            next_peer: AtomicU64::new(0),
            shared_registry: None,
        }))
    }

    /// Claude: where peers reach this clock at `local`'s IP and scope.
    pub(crate) fn endpoint(&self, local: SocketAddr) -> Endpoint {
        let mut event = local;
        event.set_port(self.event_port);
        let mut general = local;
        general.set_port(self.general_port);
        Endpoint { event, general }
    }

    pub(crate) fn identity(&self) -> ClockIdentity {
        self.identity
    }

    /// Claude: starts timing with the receiver at `peer`: following its clock
    /// when it wins the best master comparison against this clock, leading it
    /// otherwise. Dropping the `Peer` stops.
    pub(crate) fn add_peer(self: &Arc<Self>, local: SocketAddr, peer: Endpoint) -> Peer {
        let id = self.next_peer.fetch_add(1, Ordering::Relaxed);
        let timing = Arc::new(Mutex::new(Timing::Acquiring));
        if !self
            .control
            .send(runtime::Command::Add(id, local, peer, timing.clone()))
        {
            *timing.lock().unwrap() = Timing::Lost("PTP thread has stopped".into());
        }
        Peer {
            clock: self.clone(),
            id,
            timing,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_test_timeline(&self, timebase: Timebase) {
        self.control.send(runtime::Command::Timeline(timebase));
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
    Lost(String),
}

/// Claude: one receiver this clock times with.
pub(crate) struct Peer {
    clock: Arc<Ptp>,
    id: u64,
    timing: Arc<Mutex<Timing>>,
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.clock.control.send(runtime::Command::Remove(self.id));
    }
}

impl Peer {
    pub(crate) fn timing(&self) -> Timing {
        self.timing.lock().unwrap().clone()
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
        assert_eq!(
            PtpTime {
                nanos: 7_000_000_000
            }
            .frac(),
            0
        );
    }

    #[test]
    fn a_timebase_maps_host_instants_both_ways_at_its_rate() {
        let host = Instant::now() + Duration::from_secs(100);
        let network = PtpTime {
            nanos: 231_017_000_000_000,
        };
        let timebase = Timebase::new(TIMELINE, host, network, 1.0001).unwrap();
        let later = host + Duration::from_secs(10);
        assert_eq!(
            timebase.network(later).unwrap().nanos,
            network.nanos + 10_001_000_000
        );
        let earlier = host - Duration::from_secs(10);
        assert_eq!(
            timebase.network(earlier).unwrap().nanos,
            network.nanos - 10_001_000_000
        );
        for at in [later, earlier, host] {
            let back = timebase.host(timebase.network(at).unwrap()).unwrap();
            assert!(signed_nanos(back, at).abs() <= 1);
        }
    }

    #[test]
    fn standard_endpoints_keep_the_address_and_its_scope() {
        let scoped: SocketAddr = "[fe80::d85c:c0f5:c1a0:d348%1]:7000".parse().unwrap();
        let endpoint = Endpoint::standard(scoped);
        assert_eq!(
            endpoint.event.to_string(),
            "[fe80::d85c:c0f5:c1a0:d348%1]:319"
        );
        assert_eq!(
            endpoint.general.to_string(),
            "[fe80::d85c:c0f5:c1a0:d348%1]:320"
        );
    }
    #[test]
    fn time_conversions_reject_invalid_rates_and_out_of_range_values() {
        let host = Instant::now();
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(Timebase::new(TIMELINE, host, PtpTime { nanos: 0 }, rate).is_none());
        }
        let zero = Timebase::new(TIMELINE, host, PtpTime { nanos: 0 }, 1.0).unwrap();
        assert!(zero.network(host - Duration::from_nanos(1)).is_none());
        let max = Timebase::new(TIMELINE, host, PtpTime { nanos: u64::MAX }, 1.0).unwrap();
        assert_eq!(max.network(host), Some(PtpTime { nanos: u64::MAX }));
        assert!(max.network(host + Duration::from_nanos(1)).is_none());
        let slow = Timebase::new(TIMELINE, host, PtpTime { nanos: 0 }, f64::MIN_POSITIVE).unwrap();
        assert!(slow.host(PtpTime { nanos: 1 }).is_none());
        let fast = Timebase::new(TIMELINE, host, PtpTime { nanos: 0 }, f64::MAX).unwrap();
        assert!(fast.network(host + Duration::from_secs(1)).is_none());
        assert!(
            PtpTime { nanos: u64::MAX }
                .after(Duration::from_nanos(1))
                .is_none()
        );
        assert!(
            PtpTime { nanos: 0 }
                .after(Duration::from_secs(u64::MAX))
                .is_none()
        );
        assert_eq!(
            PtpTime { nanos: 1 }.after(Duration::from_nanos(2)),
            Some(PtpTime { nanos: 3 })
        );
    }
}
