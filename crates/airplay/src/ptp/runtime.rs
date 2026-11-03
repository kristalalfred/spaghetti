use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};
use tracing::{debug, info, warn};

use super::servo::{HOLDOVER, SILENCE, Sample, Servo};
use super::wire::{
    Body, Dataset, FollowInfo, Message, PortIdentity, Timestamp, Tlv, apple_signaling,
};
use super::{ClockIdentity, Config, Endpoint, Timebase, Timing};

const ELECTION: Duration = Duration::from_secs(2);
const ANNOUNCE_WITHOUT_TIMING: Duration = Duration::from_secs(3);
const EXCHANGE_LIFETIME: Duration = Duration::from_secs(1);
const ANNOUNCE_LOG: i8 = -2;
const SYNC_LOG: i8 = -3;

type Observer = Arc<Mutex<Timing>>;

pub(super) enum Command {
    Add(u64, SocketAddr, Endpoint, Observer),
    Remove(u64),
    Stop,
    #[cfg(test)]
    Timeline(Timebase),
}

pub(super) struct Control {
    pub sender: mpsc::Sender<Command>,
    pub wake: UnixDatagram,
}

impl Control {
    pub fn send(&self, command: Command) -> bool {
        if self.sender.send(command).is_err() {
            return false;
        }
        let _ = self.wake.send(&[0]);
        true
    }
}

pub(super) struct Sockets {
    sockets: [UdpSocket; 4],
    pub event_port: u16,
    pub general_port: u16,
}

impl Sockets {
    pub fn bind(config: &Config) -> Result<Self, String> {
        let event4 = bind_socket(false, config.event_port)?;
        let general4 = bind_socket(false, config.general_port)?;
        let event_port = event4.local_addr().map_err(|e| e.to_string())?.port();
        let general_port = general4.local_addr().map_err(|e| e.to_string())?.port();
        let event6 = bind_socket(true, event_port)?;
        let general6 = bind_socket(true, general_port)?;
        Ok(Self {
            sockets: [event4, general4, event6, general6],
            event_port,
            general_port,
        })
    }

    fn send(&self, local: SocketAddr, endpoint: Endpoint, message: Message) -> io::Result<Instant> {
        let event = message.body.event();
        let destination = if event {
            endpoint.event
        } else {
            endpoint.general
        };
        let index = usize::from(destination.is_ipv6()) * 2 + usize::from(!event);
        let bytes = message.encode();
        let sent = super::datagram::send(&self.sockets[index], local, destination, &bytes)?;
        debug!(%destination, kind = message.body.kind(), sequence = message.sequence, source_port = message.source.port, "PTP sent");
        Ok(sent)
    }
}

fn bind_socket(ipv6: bool, port: u16) -> Result<UdpSocket, String> {
    let address = SocketAddr::new(
        if ipv6 {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        } else {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        },
        port,
    );
    let make = || -> io::Result<UdpSocket> {
        let socket = Socket::new(
            if ipv6 { Domain::IPV6 } else { Domain::IPV4 },
            Type::DGRAM,
            Some(Protocol::UDP),
        )?;
        if ipv6 {
            socket.set_only_v6(true)?;
        }
        socket.set_nonblocking(true)?;
        socket.bind(&address.into())?;
        Ok(socket.into())
    };
    make().map_err(|e| format!("could not bind PTP UDP {address}: {e}"))
}

pub(super) fn run(
    sockets: Sockets,
    wake: UnixDatagram,
    commands: mpsc::Receiver<Command>,
    own: Timebase,
    config: Config,
) {
    let identity = own.timeline().clock;
    #[cfg(test)]
    let mut own = own;
    let mut peers: HashMap<Endpoint, Relationship> = HashMap::new();
    let mut paths: HashMap<SocketAddr, u16> = HashMap::new();
    let mut next_ports = [32777_u16, 32778_u16];
    let mut buffer = [0; 2048];
    loop {
        while let Ok(command) = commands.try_recv() {
            match command {
                Command::Stop => return,
                Command::Add(id, local, endpoint, observer) => {
                    if let Some(peer) = peers.get_mut(&endpoint) {
                        if peer.address.ip() != local.ip()
                            || matches!((peer.address, local), (SocketAddr::V6(a), SocketAddr::V6(b)) if a.scope_id() != b.scope_id())
                        {
                            *observer.lock().unwrap() =
                                Timing::Lost("PTP peer already uses another local path".into());
                            continue;
                        }
                        *observer.lock().unwrap() = peer.timing.clone();
                        peer.observers.insert(id, observer);
                        continue;
                    }
                    let route = route(local, endpoint);
                    match route {
                        Ok(route) => {
                            let port = *paths.entry(route).or_insert_with(|| {
                                let next = &mut next_ports[usize::from(route.is_ipv6())];
                                let port = *next;
                                *next = next.wrapping_add(2);
                                port
                            });
                            let source = PortIdentity {
                                clock: identity,
                                port,
                            };
                            debug!(?endpoint, %route, port, "PTP peer added");
                            peers.insert(
                                endpoint,
                                Relationship::new(
                                    endpoint,
                                    route,
                                    source,
                                    Dataset::local(own.timeline().clock, config.priority1),
                                    id,
                                    observer,
                                    Instant::now(),
                                ),
                            );
                        }
                        Err(reason) => {
                            *observer.lock().unwrap() = Timing::Lost(reason);
                        }
                    }
                }
                Command::Remove(id) => {
                    peers.retain(|_, peer| {
                        peer.observers.remove(&id);
                        !peer.observers.is_empty()
                    });
                }
                #[cfg(test)]
                Command::Timeline(timebase) => {
                    own = timebase;
                    for peer in peers.values_mut() {
                        peer.local.grandmaster = own.timeline().clock;
                        peer.next_announce = Instant::now();
                        if matches!(peer.role, Role::Leading) {
                            peer.publish(Timing::Settled(own));
                        }
                    }
                }
            }
        }
        let now = Instant::now();
        let mut deadline = now + Duration::from_secs(60);
        for peer in peers.values_mut() {
            peer.tick(now, &sockets, own);
            deadline = deadline.min(peer.deadline(now));
        }
        let mut descriptors: Vec<_> = sockets
            .sockets
            .iter()
            .map(|s| libc::pollfd {
                fd: s.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        descriptors.push(libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128) as i32;
        let ready = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                timeout,
            )
        };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            for peer in peers.values_mut() {
                peer.lose(format!("PTP socket poll failed: {error}"));
            }
            return;
        }
        if descriptors[4].revents != 0 {
            while wake.recv(&mut buffer).is_ok() {}
        }
        for (index, socket) in sockets.sockets.iter().enumerate() {
            if descriptors[index].revents == 0 {
                continue;
            }
            for _ in 0..64 {
                let result = socket.recv_from(&mut buffer);
                let received = Instant::now();
                let (length, from) = match result {
                    Ok(packet) => packet,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        debug!(error = %e, "PTP receive failed");
                        break;
                    }
                };
                let event = index % 2 == 0;
                let Some(message) = Message::decode(&buffer[..length]) else {
                    continue;
                };
                if message.body.event() != event {
                    continue;
                }
                for peer in peers.values_mut().filter(|p| {
                    if event {
                        p.endpoint.event == from
                    } else {
                        p.endpoint.general == from
                    }
                }) {
                    debug!(%from, kind = message.body.kind(), sequence = message.sequence, "PTP received");
                    peer.receive(&message, received, &sockets, own);
                }
            }
        }
    }
}

fn route(mut local: SocketAddr, endpoint: Endpoint) -> Result<SocketAddr, String> {
    if endpoint.event.is_ipv4() != endpoint.general.is_ipv4()
        || endpoint.event.ip() != endpoint.general.ip()
        || endpoint.event.port() == 0
        || endpoint.general.port() == 0
        || endpoint.event.port() == endpoint.general.port()
    {
        return Err("PTP peer needs distinct event/general ports on one address".into());
    }
    if let (SocketAddr::V6(event), SocketAddr::V6(general)) = (endpoint.event, endpoint.general)
        && event.scope_id() != general.scope_id()
    {
        return Err("PTP peer addresses have different IPv6 scopes".into());
    }
    if local.is_ipv4() != endpoint.event.is_ipv4()
        || local.ip().is_unspecified()
        || local.ip().is_multicast()
    {
        return Err("PTP local address must be a specific address in the peer's family".into());
    }
    local.set_port(0);
    UdpSocket::bind(local).map_err(|e| format!("PTP local address {local}: {e}"))?;
    Ok(local)
}

enum Role {
    Electing,
    Leading,
    Following(Box<Follower>),
    Lost,
}

struct Relationship {
    endpoint: Endpoint,
    address: SocketAddr,
    source: PortIdentity,
    local: Dataset,
    role: Role,
    observers: HashMap<u64, Observer>,
    timing: Timing,
    election_until: Instant,
    next_announce: Instant,
    next_sync: Instant,
    announce_interval: Option<Duration>,
    sync_interval: Option<Duration>,
    announce_log: i8,
    sync_log: i8,
    announce_sequence: u16,
    sync_sequence: u16,
    signaling_sequence: u16,
    epoch: u32,
    rejected_master: Option<Box<Follower>>,
}

impl Relationship {
    fn new(
        endpoint: Endpoint,
        address: SocketAddr,
        source: PortIdentity,
        local: Dataset,
        id: u64,
        observer: Observer,
        now: Instant,
    ) -> Self {
        Self {
            endpoint,
            address,
            source,
            local,
            role: Role::Electing,
            observers: HashMap::from([(id, observer)]),
            timing: Timing::Acquiring,
            election_until: now + ELECTION,
            next_announce: now,
            next_sync: now,
            announce_interval: Some(Duration::from_millis(250)),
            sync_interval: Some(Duration::from_millis(125)),
            announce_log: ANNOUNCE_LOG,
            sync_log: SYNC_LOG,
            announce_sequence: 0,
            sync_sequence: 0,
            signaling_sequence: 0,
            epoch: 0,
            rejected_master: None,
        }
    }

    fn publish(&mut self, timing: Timing) {
        if matches!(self.timing, Timing::Settled(_)) && matches!(timing, Timing::Acquiring) {
            self.lose("PTP timeline changed after settlement".into());
            return;
        }
        if self.timing == timing {
            return;
        }
        self.timing = timing.clone();
        for observer in self.observers.values() {
            *observer.lock().unwrap() = timing.clone();
        }
    }

    fn lose(&mut self, reason: String) {
        warn!(peer = ?self.endpoint, %reason, "PTP peer lost");
        self.role = Role::Lost;
        self.publish(Timing::Lost(reason));
    }

    fn lead(&mut self, now: Instant, own: Timebase) {
        if matches!(self.role, Role::Leading) {
            return;
        }
        if now < self.election_until {
            self.role = Role::Electing;
            self.next_announce = now;
            self.publish(Timing::Acquiring);
            return;
        }
        info!(peer = ?self.endpoint, clock = own.timeline().clock.as_u64(), "PTP peer settled leading");
        if matches!(self.role, Role::Following(_)) {
            info!(peer = ?self.endpoint, clock = own.timeline().clock.as_u64(), "PTP timeline changed");
        }
        self.role = Role::Leading;
        self.next_announce = now;
        self.next_sync = now;
        self.publish(Timing::Settled(own));
    }

    fn deadline(&self, now: Instant) -> Instant {
        match &self.role {
            Role::Electing => self.announce_interval.map_or(self.election_until, |_| {
                self.election_until.min(self.next_announce)
            }),
            Role::Leading => self
                .announce_interval
                .map(|_| self.next_announce)
                .into_iter()
                .chain(self.sync_interval.map(|_| self.next_sync))
                .min()
                .unwrap_or(now + Duration::from_secs(60)),
            Role::Following(f) => {
                let freshness = f.last_valid + if f.silent { HOLDOVER } else { SILENCE };
                if f.has_sync_pair {
                    freshness
                } else {
                    freshness.min(f.started + ANNOUNCE_WITHOUT_TIMING)
                }
            }
            Role::Lost => now + Duration::from_secs(60),
        }
    }

    fn tick(&mut self, now: Instant, sockets: &Sockets, own: Timebase) {
        if matches!(self.role, Role::Electing) && now >= self.election_until {
            self.lead(now, own);
        }
        if let Role::Following(f) = &mut self.role {
            if !f.has_sync_pair && now.duration_since(f.started) >= ANNOUNCE_WITHOUT_TIMING {
                self.rejected_master = Some(Box::new(Follower::new(
                    f.source,
                    f.dataset.clone(),
                    self.epoch.wrapping_add(1),
                    now,
                )));
                self.lead(now, own);
                return;
            }
            if now.duration_since(f.last_valid) >= HOLDOVER {
                self.lose("followed PTP master silent or unusable for 15 seconds".into());
                return;
            }
            if !f.silent && now.duration_since(f.last_valid) >= SILENCE {
                f.silent = true;
                warn!(peer = ?self.endpoint, clock = f.servo.timeline().clock.as_u64(), holdover = f.servo.timebase().is_some(), "PTP master silent");
            }
            return;
        }
        if matches!(self.role, Role::Lost) {
            return;
        }
        if now >= self.next_announce
            && let Some(interval) = self.announce_interval
        {
            self.announce_sequence = self.announce_sequence.wrapping_add(1);
            let mut announce = Message::new(
                self.source,
                self.announce_sequence,
                self.announce_log,
                Body::Announce {
                    origin: Timestamp::ZERO,
                    utc_offset: 37,
                    reserved: 0,
                    dataset: self.local.clone(),
                    time_source: 0xa0,
                },
            );
            announce.tlvs = vec![Tlv::Path(vec![self.local.grandmaster])];
            self.send(sockets, announce);
            let mut signaling = Message::new(
                self.source,
                self.signaling_sequence,
                -128,
                Body::Signaling {
                    target: PortIdentity {
                        clock: ClockIdentity([0; 8]),
                        port: 0,
                    },
                },
            );
            self.signaling_sequence = self.signaling_sequence.wrapping_add(1);
            signaling.tlvs = apple_signaling();
            self.send(sockets, signaling);
            self.next_announce = now + interval;
        }
        if matches!(self.role, Role::Leading)
            && now >= self.next_sync
            && let Some(interval) = self.sync_interval
        {
            self.sync_sequence = self.sync_sequence.wrapping_add(1);
            let sync = Message::new(
                self.source,
                self.sync_sequence,
                self.sync_log,
                Body::Sync(Timestamp::ZERO),
            );
            if let Some(sent) = self.send(sockets, sync) {
                let mut follow = Message::new(
                    self.source,
                    self.sync_sequence,
                    self.sync_log,
                    Body::FollowUp(Timestamp::from_nanos(match own.network(sent) {
                        Some(time) => time.nanos,
                        None => {
                            self.lose("PTP local time overflow".into());
                            return;
                        }
                    })),
                );
                follow.tlvs = vec![Tlv::Follow(FollowInfo {
                    rate_offset: 0,
                    timebase: own.timeline().epoch as u16,
                    phase_change: [0; 12],
                    frequency_change: 0,
                })];
                let mut identity = own.timeline().clock.0.to_vec();
                identity.extend([0, 0]);
                follow.tlvs.push(Tlv::Organization {
                    oui: [0, 0x0d, 0x93],
                    subtype: [0, 0, 4],
                    data: identity,
                });
                self.send(sockets, follow);
            }
            self.next_sync = now + interval;
        }
    }

    fn send(&self, sockets: &Sockets, message: Message) -> Option<Instant> {
        match sockets.send(self.address, self.endpoint, message) {
            Ok(sent) => Some(sent),
            Err(error) => {
                debug!(peer = ?self.endpoint, %error, "PTP send failed");
                None
            }
        }
    }

    fn receive(&mut self, message: &Message, received: Instant, sockets: &Sockets, own: Timebase) {
        if matches!(self.role, Role::Lost) || message.source.clock == self.source.clock {
            return;
        }
        if matches!(
            message.body,
            Body::Sync(_) | Body::FollowUp(_) | Body::DelayResp { .. }
        ) && let Some(candidate) = &mut self.rejected_master
            && message.source == candidate.source
        {
            if let Err(reason) = candidate.receive(
                message,
                received,
                sockets,
                self.address,
                self.endpoint,
                self.source,
            ) {
                self.lose(reason.into());
                return;
            }
            if candidate.has_sync_pair {
                self.epoch = candidate.servo.timeline().epoch;
                info!(peer = ?self.endpoint, clock = candidate.dataset.grandmaster.as_u64(), epoch = self.epoch, "PTP rejected master resumed timing; acquiring follower");
                self.role = Role::Following(self.rejected_master.take().unwrap());
                self.publish(Timing::Acquiring);
            }
            return;
        }
        match &message.body {
            Body::Announce { dataset, .. } => {
                if dataset.grandmaster == self.local.grandmaster || dataset.steps >= 255 {
                    return;
                }
                if dataset < &self.local {
                    if self.rejected_master.as_ref().is_some_and(|candidate| {
                        candidate.source == message.source && &candidate.dataset == dataset
                    }) {
                        return;
                    }
                    self.rejected_master = None;
                    let changed = !matches!(&self.role, Role::Following(f) if f.source == message.source && f.servo.timeline().clock == dataset.grandmaster);
                    if changed {
                        self.epoch = self.epoch.wrapping_add(1);
                        info!(peer = ?self.endpoint, clock = dataset.grandmaster.as_u64(), epoch = self.epoch, "PTP timeline changed; acquiring follower");
                        self.role = Role::Following(Box::new(Follower::new(
                            message.source,
                            dataset.clone(),
                            self.epoch,
                            received,
                        )));
                        self.publish(Timing::Acquiring);
                    } else if let Role::Following(follower) = &mut self.role {
                        follower.dataset = dataset.clone();
                    }
                } else {
                    self.rejected_master = None;
                    self.lead(received, own);
                }
            }
            Body::Signaling { target } => {
                if target.clock != ClockIdentity([0; 8])
                    && target.clock != ClockIdentity([0xff; 8])
                    && *target != self.source
                {
                    return;
                }
                for tlv in &message.tlvs {
                    if let Tlv::Interval { sync, announce, .. } = tlv {
                        set_interval(*sync, SYNC_LOG, &mut self.sync_log, &mut self.sync_interval);
                        set_interval(
                            *announce,
                            ANNOUNCE_LOG,
                            &mut self.announce_log,
                            &mut self.announce_interval,
                        );
                        self.next_sync = received;
                        self.next_announce = received;
                    }
                }
            }
            Body::DelayReq(_) if matches!(self.role, Role::Leading) => {
                let mut response = Message::new(
                    self.source,
                    message.sequence,
                    self.sync_log,
                    Body::DelayResp {
                        received: Timestamp::from_nanos(match own.network(received) {
                            Some(time) => time.nanos,
                            None => {
                                self.lose("PTP local time overflow".into());
                                return;
                            }
                        }),
                        requester: message.source,
                    },
                );
                response.correction = message.correction;
                response.flags |= 0x0200;
                self.send(sockets, response);
            }
            _ => {
                let Role::Following(follower) = &mut self.role else {
                    return;
                };
                if message.source != follower.source {
                    return;
                }
                let previous_timeline = follower.servo.timeline();
                if let Err(reason) = follower.receive(
                    message,
                    received,
                    sockets,
                    self.address,
                    self.endpoint,
                    self.source,
                ) {
                    self.lose(reason.into());
                    return;
                }
                if follower.servo.timeline() != previous_timeline {
                    self.epoch = follower.servo.timeline().epoch;
                    info!(peer = ?self.endpoint, clock = follower.servo.timeline().clock.as_u64(), epoch = self.epoch, "PTP timeline changed");
                }
                let timing = follower
                    .servo
                    .timebase()
                    .map_or(Timing::Acquiring, Timing::Settled);
                if matches!(timing, Timing::Settled(_))
                    && !matches!(self.timing, Timing::Settled(_))
                    && let Some(q) = follower.servo.quality
                {
                    info!(peer = ?self.endpoint, clock = follower.servo.timeline().clock.as_u64(), offset_uncertainty_ns = q.offset_ns, asymmetry_bound_ns = q.asymmetry_ns, rate_uncertainty_ppm = q.rate_ppm, rate_source = ?q.rate_source, samples = q.samples, "PTP peer settled following");
                }
                self.publish(timing);
            }
        }
    }
}

fn set_interval(request: i8, default: i8, log: &mut i8, interval: &mut Option<Duration>) {
    match request {
        -128 => (),
        127 => *interval = None,
        126 => {
            *log = default;
            *interval = Some(Duration::from_secs_f64(2_f64.powi(i32::from(default))));
        }
        -7..=4 => {
            *log = request;
            *interval = Some(Duration::from_secs_f64(2_f64.powi(i32::from(request))));
        }
        _ => (),
    }
}

struct Exchange {
    created: Instant,
    sync: Option<(Instant, i64)>,
    follow: Option<(u64, i64, Option<u16>)>,
    request: Option<(u16, Instant)>,
    response: Option<(u64, i64)>,
    epoch: Option<u32>,
}

struct Follower {
    source: PortIdentity,
    dataset: Dataset,
    started: Instant,
    has_sync_pair: bool,
    servo: Servo,
    pending: HashMap<u16, Exchange>,
    recent: VecDeque<u16>,
    latest_sync: Option<u16>,
    latest_pair: Option<u16>,
    delay_sequence: u16,
    last_valid: Instant,
    last_sample: Option<Instant>,
    silent: bool,
}

impl Follower {
    fn new(source: PortIdentity, dataset: Dataset, epoch: u32, now: Instant) -> Self {
        Self {
            source,
            servo: Servo::new(dataset.grandmaster, epoch),
            dataset,
            started: now,
            has_sync_pair: false,
            pending: HashMap::new(),
            recent: VecDeque::new(),
            latest_sync: None,
            latest_pair: None,
            delay_sequence: 0,
            last_valid: now,
            last_sample: None,
            silent: false,
        }
    }

    fn receive(
        &mut self,
        message: &Message,
        received: Instant,
        sockets: &Sockets,
        address: SocketAddr,
        endpoint: Endpoint,
        local: PortIdentity,
    ) -> Result<(), &'static str> {
        self.pending
            .retain(|_, p| received.duration_since(p.created) < EXCHANGE_LIFETIME);
        let sequence = message.sequence;
        if let Body::DelayResp {
            received: time,
            requester,
        } = message.body
        {
            if requester != local {
                return Ok(());
            }
            let Some(time) = time.nanos() else {
                return Ok(());
            };
            let Some((&sync_sequence, exchange)) = self
                .pending
                .iter_mut()
                .find(|(_, e)| e.request.is_some_and(|(seq, _)| seq == sequence))
            else {
                return Ok(());
            };
            if exchange.response.is_none() {
                exchange.response = Some((time, message.correction));
            }
            self.complete(sync_sequence, received)?;
            return Ok(());
        }
        if !matches!(message.body, Body::Sync(_) | Body::FollowUp(_))
            || self.recent.contains(&sequence)
        {
            return Ok(());
        }
        if let Some(latest) = self.latest_sync {
            let delta = sequence.wrapping_sub(latest) as i16;
            if delta < -16 {
                return Ok(());
            }
        }
        if self.pending.len() >= 32 && !self.pending.contains_key(&sequence) {
            return Ok(());
        }
        let exchange = self.pending.entry(sequence).or_insert(Exchange {
            created: received,
            sync: None,
            follow: None,
            request: None,
            response: None,
            epoch: None,
        });
        match &message.body {
            Body::Sync(origin) if exchange.sync.is_none() => {
                if self
                    .latest_sync
                    .is_none_or(|latest| (sequence.wrapping_sub(latest) as i16) > 0)
                {
                    self.latest_sync = Some(sequence);
                }
                exchange.sync = Some((received, message.correction));
                if message.flags & 0x0200 == 0 {
                    exchange.follow = origin.nanos().map(|t| (t, 0, None));
                }
                self.delay_sequence = self.delay_sequence.wrapping_add(1);
                let request = Message::new(
                    local,
                    self.delay_sequence,
                    SYNC_LOG,
                    Body::DelayReq(Timestamp::ZERO),
                );
                match sockets.send(address, endpoint, request) {
                    Ok(sent) => exchange.request = Some((self.delay_sequence, sent)),
                    Err(error) => debug!(?endpoint, %error, "PTP Delay_Req failed"),
                }
            }
            Body::FollowUp(origin) if exchange.follow.is_none() => {
                let indicator = message.tlvs.iter().find_map(|tlv| match tlv {
                    Tlv::Follow(info) => Some(info.timebase),
                    _ => None,
                });
                exchange.follow = origin
                    .nanos()
                    .map(|time| (time, message.correction, indicator));
            }
            _ => (),
        }
        self.complete(sequence, received)
    }

    fn complete(&mut self, sequence: u16, received: Instant) -> Result<(), &'static str> {
        let Some(exchange) = self.pending.get_mut(&sequence) else {
            return Ok(());
        };
        if exchange.sync.is_some()
            && let Some((_, _, indicator)) = exchange.follow
            && self
                .latest_pair
                .is_none_or(|latest| (sequence.wrapping_sub(latest) as i16) > 0)
        {
            self.has_sync_pair = true;
            self.latest_pair = Some(sequence);
            if let Some(indicator) = indicator {
                self.servo.indicator(indicator);
            }
            exchange.epoch = Some(self.servo.timeline().epoch);
        }
        let (
            Some((t2, sync_correction)),
            Some((t1, follow_correction, _)),
            Some((_, t3)),
            Some((t4, reverse_correction)),
        ) = (
            exchange.sync,
            exchange.follow,
            exchange.request,
            exchange.response,
        )
        else {
            return Ok(());
        };
        let epoch = exchange.epoch;
        self.pending.remove(&sequence);
        self.recent.push_back(sequence);
        while self.recent.len() > 64 {
            self.recent.pop_front();
        }
        if epoch != Some(self.servo.timeline().epoch) {
            return Ok(());
        }
        let Some(sample) = Sample::exchange(
            t1,
            t2,
            t3,
            t4,
            i128::from(sync_correction) + i128::from(follow_correction),
            reverse_correction,
            self.servo.rate(),
        ) else {
            return Ok(());
        };
        if self.last_sample.is_some_and(|last| sample.host <= last) {
            return Ok(());
        }
        self.last_sample = Some(sample.host);
        let previous = self.servo.last_good;
        self.servo.observe(sample)?;
        if self.servo.last_good != previous && self.servo.last_good.is_some()
            || self.servo.timebase().is_none()
        {
            self.last_valid = received;
            self.silent = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
