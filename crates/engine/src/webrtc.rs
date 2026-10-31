use std::collections::{HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use spaghetti_common::ipc_protocol::{FlowState, FlowStatus, PeerRole, TurnServer, WebRtcPeer};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::channel::{ChannelConfig, ChannelData, ChannelId, Reliability};
use str0m::format::{Codec, FormatParams};
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive, Transmit};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig, RtcError};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::clock::{Capture, ClockMessage, SenderClock, now_nanos};
use crate::flow::FlowMeter;
use crate::host_time;
use crate::stun;
use crate::turn::{Received, Relays};

/// Claude: covers DNS, STUN and a TURN allocation's two round trips, the
/// challenge and the signed request.
const GATHER_DEADLINE: Duration = Duration::from_secs(2);
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
const PATH_WINDOW: Duration = Duration::from_secs(2);
const RELEASE_WAIT: Duration = Duration::from_millis(500);
/// Claude: how long str0m holds the packets behind a lost one. Its default, a
/// second over 15 packets, stalls audio up to 280 ms after every loss.
pub const REORDER_TIMEOUT: Duration = Duration::from_millis(40);
const OPUS_PT: u8 = 111;
const DATAGRAM_MAX: usize = 2048;
const ARRIVALS_MAX: usize = 256;
const CLOSE_PASSES: usize = 4;
/// Claude: the data channel on which a receiver learns its sender's clock.
const CLOCK_CHANNEL: &str = "clock";
/// Claude: a receiver probes this often at first, so the clock is known soon
/// after the link connects, and every `PROBE_INTERVAL` once it has
/// `QUICK_PROBES` round trips.
const QUICK_PROBE_INTERVAL: Duration = Duration::from_millis(100);
const QUICK_PROBES: u32 = 10;
const PROBE_INTERVAL: Duration = Duration::from_secs(1);

static CRYPTO: Once = Once::new();

/// Claude: one Opus packet on its way out: its RTP time at 48 kHz, when its
/// audio was captured, and the host time in nanoseconds its first frame was
/// captured when the source knows it.
pub struct OutgoingPacket {
    pub rtp_time: u64,
    pub captured: Instant,
    pub captured_nanos: Option<u64>,
    pub data: Vec<u8>,
}

/// Claude: one Opus packet as str0m released it, with its RTP time at 48 kHz.
pub struct IncomingPacket {
    pub rtp_time: u64,
    pub data: Arc<[u8]>,
}

/// Claude: the media a link carries, and which way. A receiver learns the
/// sender's clock into `clock` as the link runs.
pub enum Media {
    Send(mpsc::Receiver<OutgoingPacket>),
    Receive {
        packets: SyncSender<IncomingPacket>,
        clock: Arc<SenderClock>,
    },
}

impl Media {
    fn is_send(&self) -> bool {
        matches!(self, Self::Send(_))
    }

    /// Claude: the next packet to send; never resolves for a receiver.
    async fn outgoing(&mut self) -> Option<OutgoingPacket> {
        match self {
            Self::Send(packets) => packets.recv().await,
            Self::Receive { .. } => std::future::pending().await,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Claude: from creation until `Event::Connected`.
    Connecting,
    Connected,
    /// Claude: a receiver's media arrives, or has `paused` per `StreamPaused`.
    Receiving {
        paused: bool,
    },
    Unreachable,
    Lost,
}

impl Phase {
    fn is_final(self) -> bool {
        matches!(self, Self::Unreachable | Self::Lost)
    }
}

struct State {
    phase: Phase,
    peer: Option<SocketAddr>,
    relayed: bool,
    local_description: Option<String>,
    remote_description: Option<String>,
}

struct Shared {
    meter: FlowMeter,
    state: Mutex<State>,
}

impl Shared {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Claude: moves to `phase` unless the link already ended.
    fn enter(&self, phase: Phase) {
        let mut state = self.state();
        if !state.phase.is_final() {
            state.phase = phase;
        }
    }
}

/// Claude: an offer as the session applies it, with the mid of its audio line.
struct Offered {
    offer: SdpOffer,
    mid: Mid,
    peers: Vec<IpAddr>,
}

struct Answered {
    answer: SdpAnswer,
    peers: Vec<IpAddr>,
}

/// Claude: how the controller's remote description reaches the session: an
/// offerer takes an answer, an answerer an offer.
enum Signal {
    Answer(mpsc::UnboundedSender<Answered>),
    Offer(mpsc::UnboundedSender<Offered>),
}

/// Claude: one WebRTC peer connection on its own UDP socket, run by a tokio
/// task. Dropping it closes the connection, so the remote sees it end at once.
pub struct PeerLink {
    generation: u32,
    sending: bool,
    shared: Arc<Shared>,
    signal: Signal,
    _closing: DropGuard,
}

impl PeerLink {
    /// Claude: must be called within a tokio runtime, which runs the session.
    pub fn open(spec: &WebRtcPeer, media: Media) -> Result<Self, String> {
        CRYPTO.call_once(|| str0m::crypto::from_feature_flags().install_process_default());
        for server in &spec.stun {
            if !is_host_port(server) {
                return Err(format!("STUN server '{server}' is not host:port"));
            }
        }
        for server in &spec.turn {
            if !is_host_port(&server.address) {
                return Err(format!("TURN server '{}' is not host:port", server.address));
            }
        }
        let mut addresses = interface_ipv4s();
        let primary = default_route_ipv4()
            .ok()
            .or_else(|| addresses.first().copied())
            .ok_or("no IPv4 address for WebRTC")?;
        addresses.retain(|address| *address != primary);
        let (socket, local) = bind(primary)?;
        let others: Vec<_> = addresses
            .into_iter()
            .filter_map(|address| {
                bind(address)
                    .inspect_err(
                        |e| tracing::debug!(link = spec.link, error = %e, "skipped an interface"),
                    )
                    .ok()
            })
            .map(|(socket, local)| (Arc::new(socket), local))
            .collect();

        let shared = Arc::new(Shared {
            meter: FlowMeter::new(),
            state: Mutex::new(State {
                phase: Phase::Connecting,
                peer: None,
                relayed: false,
                local_description: None,
                remote_description: None,
            }),
        });
        let closing = CancellationToken::new();
        let sending = media.is_send();
        let session = Gathering {
            link: spec.link.clone(),
            socket,
            local,
            others,
            relay_only: relay_only(&spec.link),
            stun: spec.stun.clone(),
            turn: spec.turn.clone(),
            media,
            shared: Arc::clone(&shared),
            closing: closing.clone(),
        };
        let signal = match spec.role {
            PeerRole::Offer => {
                let (answers, received) = mpsc::unbounded_channel();
                tokio::spawn(session.offer(received));
                Signal::Answer(answers)
            }
            PeerRole::Answer => {
                let (offers, received) = mpsc::unbounded_channel();
                tokio::spawn(session.answer(received));
                Signal::Offer(offers)
            }
        };
        tracing::info!(link = spec.link, role = ?spec.role, %local, "opened WebRTC peer");
        Ok(Self {
            generation: spec.generation,
            sending,
            shared,
            signal,
            _closing: closing.drop_guard(),
        })
    }

    pub fn local_description(&self) -> Option<String> {
        self.shared.state().local_description.clone()
    }

    /// Claude: an offerer takes the answer to its offer, an answerer the offer it
    /// answers. Each takes one per generation; the same one again changes nothing.
    pub fn set_remote_description(&self, generation: u32, sdp: String) -> Result<(), String> {
        if generation != self.generation {
            return Err(format!(
                "generation {generation} is not the peer's generation {}",
                self.generation
            ));
        }
        let mut state = self.shared.state();
        match state.phase {
            Phase::Unreachable => return Err("the peer is unreachable".into()),
            Phase::Lost => return Err("the peer is lost".into()),
            _ => {}
        }
        if let Some(applied) = &state.remote_description {
            return if *applied == sdp {
                Ok(())
            } else {
                Err(format!(
                    "generation {generation} already has a different remote description"
                ))
            };
        }
        let peers = candidate_ips(&sdp);
        let sent = match &self.signal {
            Signal::Answer(answers) => {
                if state.local_description.is_none() {
                    return Err("the offer is not ready yet".into());
                }
                let answer = SdpAnswer::from_sdp_string(&sdp)
                    .map_err(|e| format!("the answer does not parse: {e}"))?;
                let lines = answer.media_lines.iter();
                audio_mid(
                    lines.map(|line| (line.typ.to_string(), line.direction(), line.mid())),
                    self.sending,
                )?;
                answers.send(Answered { answer, peers }).is_ok()
            }
            Signal::Offer(offers) => {
                let offer = SdpOffer::from_sdp_string(&sdp)
                    .map_err(|e| format!("the offer does not parse: {e}"))?;
                let lines = offer.media_lines.iter();
                let mid = audio_mid(
                    lines.map(|line| (line.typ.to_string(), line.direction(), line.mid())),
                    self.sending,
                )?;
                offers.send(Offered { offer, mid, peers }).is_ok()
            }
        };
        if !sent {
            return Err("the peer has stopped".into());
        }
        state.remote_description = Some(sdp);
        Ok(())
    }

    /// Claude: `packets_lost` is the receiver's to count, so it is 0 here.
    pub fn status(&self) -> FlowStatus {
        let state = self.shared.state();
        let flow = match state.phase {
            Phase::Connecting => FlowState::Connecting,
            Phase::Connected if self.sending => self.shared.meter.progress(),
            Phase::Connected => FlowState::Connected,
            Phase::Receiving { paused: false } => FlowState::Flowing,
            Phase::Receiving { paused: true } => FlowState::Stalled,
            Phase::Unreachable => FlowState::Unreachable,
            Phase::Lost => FlowState::Lost,
        };
        let connected = matches!(state.phase, Phase::Connected | Phase::Receiving { .. });
        FlowStatus {
            state: flow,
            bytes: self.shared.meter.bytes(),
            peer: state
                .peer
                .filter(|_| connected)
                .map(|peer| peer.to_string()),
            relayed: connected && state.relayed,
            packets_lost: 0,
            packets_retransmitted: 0,
        }
    }
}

fn is_host_port(server: &str) -> bool {
    server
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok())
}

fn candidate_ips(sdp: &str) -> Vec<IpAddr> {
    sdp.lines()
        .filter_map(|line| line.strip_prefix("a="))
        .filter(|attribute| attribute.starts_with("candidate:"))
        .filter_map(|attribute| Candidate::from_sdp_string(attribute).ok())
        .map(|candidate| candidate.addr().ip())
        .collect()
}

/// Claude: test-only; these links offer only relayed candidates, so one host
/// can force the path through a TURN server.
#[cfg(test)]
pub static RELAY_ONLY: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[cfg(test)]
fn relay_only(link: &str) -> bool {
    RELAY_ONLY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .any(|forced| forced == link)
}

#[cfg(not(test))]
fn relay_only(_: &str) -> bool {
    false
}

/// Claude: the mid of the remote's audio line, given its media lines as (kind,
/// direction, mid), when that line carries audio the way this peer needs it.
fn audio_mid(
    mut lines: impl Iterator<Item = (String, Direction, Mid)>,
    sending: bool,
) -> Result<Mid, String> {
    let (_, direction, mid) = lines
        .find(|(kind, ..)| kind == "audio")
        .ok_or("the remote description has no audio")?;
    if sending && !direction.is_receiving() {
        return Err(format!(
            "the remote's audio is {direction}, so it does not receive"
        ));
    }
    if !sending && !direction.is_sending() {
        return Err(format!(
            "the remote's audio is {direction}, so it does not send"
        ));
    }
    Ok(mid)
}

fn bind(address: Ipv4Addr) -> Result<(UdpSocket, SocketAddr), String> {
    let socket = std::net::UdpSocket::bind((address, 0))
        .and_then(|socket| socket.set_nonblocking(true).map(|()| socket))
        .map_err(|e| format!("bind a WebRTC socket on {address}: {e}"))?;
    let local = socket
        .local_addr()
        .map_err(|e| format!("WebRTC socket address: {e}"))?;
    let socket =
        UdpSocket::from_std(socket).map_err(|e| format!("register a WebRTC socket: {e}"))?;
    Ok((socket, local))
}

/// Claude: the IPv4 address of every interface that is up, except loopback
/// and link-local ones. A link offers a host candidate on each, so a peer
/// reaches it on a network other than the default route's, such as the LAN
/// while a VPN holds the default route.
pub fn interface_ipv4s() -> Vec<Ipv4Addr> {
    let mut addresses = Vec::new();
    let mut interfaces = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut interfaces) } != 0 {
        return addresses;
    }
    let mut cursor = interfaces;
    while let Some(interface) = unsafe { cursor.as_ref() } {
        cursor = interface.ifa_next;
        let flags = interface.ifa_flags as libc::c_int;
        if flags & libc::IFF_UP == 0
            || flags & libc::IFF_RUNNING == 0
            || flags & libc::IFF_LOOPBACK != 0
        {
            continue;
        }
        let Some(address) = (unsafe { interface.ifa_addr.as_ref() }) else {
            continue;
        };
        if libc::c_int::from(address.sa_family) != libc::AF_INET {
            continue;
        }
        let address = unsafe { &*interface.ifa_addr.cast::<libc::sockaddr_in>() };
        let ip = Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr));
        if !ip.is_link_local() && !ip.is_unspecified() && !addresses.contains(&ip) {
            addresses.push(ip);
        }
    }
    unsafe { libc::freeifaddrs(interfaces) };
    addresses
}

/// Claude: the address this host reaches the internet from. Connecting a UDP
/// socket only picks the route; nothing is sent.
pub fn default_route_ipv4() -> Result<Ipv4Addr, String> {
    let probe = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|probe| {
            probe
                .connect((Ipv4Addr::new(192, 0, 2, 1), 9))
                .map(|()| probe)
        })
        .and_then(|probe| probe.local_addr())
        .map_err(|e| format!("no default route for WebRTC: {e}"))?;
    match probe.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() => Ok(ip),
        ip => Err(format!("the default route has no IPv4 address ({ip})")),
    }
}

fn rtc_config() -> RtcConfig {
    let mut config = RtcConfig::new()
        .clear_codecs()
        .set_reordering_timeout_audio(Some(REORDER_TIMEOUT));
    config.codec_config().add_config(
        Pt::from(OPUS_PT),
        None,
        Codec::Opus,
        Frequency::FORTY_EIGHT_KHZ,
        Some(2),
        FormatParams {
            min_p_time: Some(10),
            use_inband_fec: Some(true),
            stereo: Some(true),
            sprop_stereo: Some(true),
            ..FormatParams::default()
        },
    );
    config
}

struct Gathered {
    issued: HashSet<stun::Transaction>,
    mapped: Vec<SocketAddrV4>,
    relays: Relays,
}

/// Claude: asks every STUN server, from the media socket itself, how the
/// internet sees it, and every TURN server for an allocation on it. Ends when
/// all have answered or `GATHER_DEADLINE` passed.
async fn gather(
    socket: &UdpSocket,
    stun_servers: &[String],
    turn_servers: &[TurnServer],
    link: &str,
) -> Gathered {
    let deadline = Instant::now() + GATHER_DEADLINE;
    let (stun_addresses, turn_addresses) = tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        futures::future::join(
            futures::future::join_all(stun_servers.iter().map(|server| resolve(server))),
            futures::future::join_all(turn_servers.iter().map(|server| resolve(&server.address))),
        ),
    )
    .await
    .unwrap_or_default();
    // Claude: tokio refuses `try_send_to` on a new socket until its reactor
    // has seen it writable.
    if let Err(e) = socket.writable().await {
        tracing::warn!(link, error = %e, "the WebRTC socket is not writable");
    }
    let mut issued = HashSet::new();
    let mut waiting = Vec::new();
    for (server, address) in stun_servers.iter().zip(stun_addresses) {
        let Some(address) = address else {
            tracing::warn!(link, server, "cannot resolve STUN server");
            continue;
        };
        let transaction = stun::transaction_id();
        if send(socket, address, &stun::binding_request(transaction), link) {
            issued.insert(transaction);
            waiting.push(transaction);
        } else {
            tracing::warn!(link, server, "STUN request failed");
        }
    }
    let mut turn = Vec::new();
    for (server, address) in turn_servers.iter().zip(turn_addresses) {
        match address {
            Some(address) => turn.push((address, server)),
            None => tracing::info!(
                link,
                server = server.address,
                "cannot resolve TURN server; no relayed candidate"
            ),
        }
    }
    let mut relays = Relays::new(link, turn, Instant::now());
    let mut mapped = Vec::new();
    let mut buffer = [0u8; DATAGRAM_MAX];
    loop {
        let now = Instant::now();
        relays.handle_timeout(now);
        flush(socket, &mut relays, link);
        if (waiting.is_empty() && relays.settled()) || now >= deadline {
            break;
        }
        let wake = relays.timeout().map_or(deadline, |at| at.min(deadline));
        let Ok(Ok((len, from))) = tokio::time::timeout_at(
            tokio::time::Instant::from_std(wake),
            socket.recv_from(&mut buffer),
        )
        .await
        else {
            continue;
        };
        let datagram = &buffer[..len];
        let answered = stun::transaction(datagram)
            .and_then(|transaction| waiting.iter().position(|t| *t == transaction));
        let Some(index) = answered else {
            relays.receive(datagram, from, Instant::now());
            continue;
        };
        if let Some(address) = stun::mapped_address(datagram, waiting[index]) {
            waiting.swap_remove(index);
            if !mapped.contains(&address) {
                mapped.push(address);
            }
        }
    }
    if !waiting.is_empty() {
        tracing::info!(
            link,
            unanswered = waiting.len(),
            "STUN servers did not answer in time"
        );
    }
    relays.abandon_unanswered();
    Gathered {
        issued,
        mapped,
        relays,
    }
}

fn send(socket: &UdpSocket, to: SocketAddr, datagram: &[u8], link: &str) -> bool {
    match socket.try_send_to(datagram, to) {
        Ok(_) => true,
        Err(e) => {
            tracing::debug!(link, destination = %to, error = %e, "WebRTC send failed");
            false
        }
    }
}

fn flush(socket: &UdpSocket, relays: &mut Relays, link: &str) {
    while let Some((server, datagram)) = relays.poll_transmit() {
        send(socket, server, &datagram, link);
    }
}

/// Claude: IPv4 only, like the media socket.
async fn resolve(server: &str) -> Option<SocketAddr> {
    tokio::net::lookup_host(server)
        .await
        .ok()?
        .find(SocketAddr::is_ipv4)
}

/// Claude: a link before its session exists: the socket, and what it needs to
/// gather and describe itself.
struct Gathering {
    link: String,
    socket: UdpSocket,
    local: SocketAddr,
    /// Claude: a socket on every other interface, each offered as a host
    /// candidate. STUN and TURN use only `socket`.
    others: Vec<(Arc<UdpSocket>, SocketAddr)>,
    relay_only: bool,
    stun: Vec<String>,
    turn: Vec<TurnServer>,
    media: Media,
    shared: Arc<Shared>,
    closing: CancellationToken,
}

impl Gathering {
    async fn offer(self, answers: mpsc::UnboundedReceiver<Answered>) {
        let gathered = gather(&self.socket, &self.stun, &self.turn, &self.link).await;
        let mut rtc = self.rtc(&gathered);
        let direction = if self.media.is_send() {
            Direction::SendOnly
        } else {
            Direction::RecvOnly
        };
        let mut changes = rtc.sdp_api();
        let mid = changes.add_media(MediaKind::Audio, direction, None, None, None);
        changes.add_channel_with_config(ChannelConfig {
            label: CLOCK_CHANNEL.into(),
            ordered: false,
            reliability: Reliability::MaxRetransmits { retransmits: 0 },
            ..ChannelConfig::default()
        });
        let (offer, pending) = changes.apply().expect("new media needs an offer");
        let mut session = self.into_session(rtc, gathered, mid);
        session.pending = Some(pending);
        session.answers = Some(answers);
        session.publish(offer.to_sdp_string());
        session.run().await;
    }

    async fn answer(mut self, mut offers: mpsc::UnboundedReceiver<Offered>) {
        let mut buffer = [0u8; DATAGRAM_MAX];
        let Offered { offer, mid, peers } = loop {
            tokio::select! {
                () = self.closing.cancelled() => return,
                offered = offers.recv() => match offered {
                    Some(offered) => break offered,
                    None => return,
                },
                _ = self.socket.recv_from(&mut buffer) => {}
                Some(_) = self.media.outgoing() => {}
            }
        };
        let gathered = gather(&self.socket, &self.stun, &self.turn, &self.link).await;
        let mut rtc = self.rtc(&gathered);
        let accepted = rtc.sdp_api().accept_offer(offer);
        let mut session = self.into_session(rtc, gathered, mid);
        match accepted {
            Ok(answer) => {
                session.permit(&peers);
                session.publish(answer.to_sdp_string());
                session.run().await;
            }
            Err(e) => {
                session.end(Phase::Lost, &format!("the offer was not accepted: {e}"));
                session.close().await;
            }
        }
    }

    fn rtc(&self, gathered: &Gathered) -> Rtc {
        let mut rtc = rtc_config().build(Instant::now());
        let mut candidates = Vec::new();
        if !self.relay_only {
            candidates.push(Candidate::host(self.local, "udp"));
            for (_, local) in &self.others {
                candidates.push(Candidate::host(*local, "udp"));
            }
            for &address in &gathered.mapped {
                let address = SocketAddr::V4(address);
                if address != self.local {
                    candidates.push(Candidate::server_reflexive(address, self.local, "udp"));
                }
            }
        }
        for address in gathered.relays.relayed() {
            candidates.push(Candidate::relayed(address, self.local, "udp"));
        }
        for candidate in candidates {
            match candidate {
                Ok(candidate) => {
                    rtc.add_local_candidate(candidate);
                }
                Err(e) => {
                    tracing::debug!(link = self.link, error = %e, "skipped a candidate")
                }
            }
        }
        rtc
    }

    fn into_session(self, rtc: Rtc, gathered: Gathered, mid: Mid) -> Session {
        let readers = self.closing.child_token();
        let (arrive, arrivals) = mpsc::channel(ARRIVALS_MAX);
        for (socket, local) in &self.others {
            tokio::spawn(read(
                Arc::clone(socket),
                *local,
                arrive.clone(),
                readers.clone(),
            ));
        }
        Session {
            link: self.link,
            rtc,
            socket: self.socket,
            local: self.local,
            others: self.others,
            arrivals,
            _readers: readers.drop_guard(),
            issued: gathered.issued,
            relays: gathered.relays,
            mid,
            media: self.media,
            shared: self.shared,
            closing: self.closing,
            pending: None,
            answers: None,
            connect_by: None,
            connected: false,
            paths: RecentPaths::default(),
            pt: None,
            clock: None,
            probes: 0,
            probe_at: None,
            latest: None,
        }
    }
}

/// Claude: most datagrams accepted within `PATH_WINDOW` decide, so a path
/// change shows within it and a stray check on another pair does not.
#[derive(Default)]
struct RecentPaths {
    arrivals: VecDeque<(Instant, bool)>,
    relayed: usize,
}

impl RecentPaths {
    fn record(&mut self, now: Instant, relayed: bool) -> bool {
        self.arrivals.push_back((now, relayed));
        self.relayed += usize::from(relayed);
        while let Some(&(at, was_relayed)) = self.arrivals.front()
            && now.duration_since(at) > PATH_WINDOW
        {
            self.arrivals.pop_front();
            self.relayed -= usize::from(was_relayed);
        }
        self.relayed * 2 > self.arrivals.len()
    }
}

/// Claude: a running str0m session. The task that owns it is the only one
/// touching the `Rtc` and the socket.
struct Session {
    link: String,
    rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    others: Vec<(Arc<UdpSocket>, SocketAddr)>,
    /// Claude: datagrams the other sockets received.
    arrivals: mpsc::Receiver<Arrival>,
    _readers: DropGuard,
    /// Claude: our own STUN transactions; their replies never reach str0m.
    issued: HashSet<stun::Transaction>,
    relays: Relays,
    mid: Mid,
    media: Media,
    shared: Arc<Shared>,
    closing: CancellationToken,
    /// Claude: an offerer's offer until its answer arrives on `answers`.
    pending: Option<SdpPendingOffer>,
    answers: Option<mpsc::UnboundedReceiver<Answered>>,
    /// Claude: an offerer that applied its answer must connect by then.
    connect_by: Option<Instant>,
    connected: bool,
    paths: RecentPaths,
    pt: Option<Pt>,
    /// Claude: the clock channel, once open.
    clock: Option<ChannelId>,
    /// Claude: a receiver's probes so far, and when it sends the next.
    probes: u32,
    probe_at: Option<Instant>,
    /// Claude: a sender's latest packet with a known capture time.
    latest: Option<Capture>,
}

impl Session {
    fn publish(&self, sdp: String) {
        tracing::info!(link = self.link, "WebRTC local description ready");
        self.shared.state().local_description = Some(sdp);
    }

    async fn run(mut self) {
        let mut buffer = vec![0u8; DATAGRAM_MAX];
        loop {
            let timeout = match self.drain() {
                Ok(timeout) => timeout,
                Err(e) => {
                    self.end(Phase::Lost, &format!("str0m failed: {e}"));
                    break;
                }
            };
            if self.shared.state().phase.is_final() {
                break;
            }
            let wake = [self.connect_by, self.relays.timeout(), self.probe_at]
                .into_iter()
                .flatten()
                .fold(timeout, Instant::min);
            tokio::select! {
                () = self.closing.cancelled() => break,
                Some(answer) = next(&mut self.answers) => self.accept_answer(answer),
                Some(packet) = self.media.outgoing() => self.write(packet),
                received = self.socket.recv_from(&mut buffer) => match received {
                    Ok((len, from)) => self.receive(&buffer[..len], from),
                    Err(e) => tracing::debug!(link = self.link, error = %e, "WebRTC receive failed"),
                },
                Some(arrival) = self.arrivals.recv() => {
                    self.deliver(&arrival.datagram, arrival.from, arrival.to);
                }
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => self.tick(),
            }
        }
        self.close().await;
    }

    /// Claude: hands str0m's pending transmits to the socket and acts on its
    /// events, then returns when it next wants a timeout.
    fn drain(&mut self) -> Result<Instant, RtcError> {
        loop {
            match self.rtc.poll_output()? {
                Output::Timeout(at) => return Ok(at),
                Output::Transmit(transmit) => self.transmit(transmit),
                Output::Event(event) => self.on_event(event),
            }
        }
    }

    /// Claude: every datagram str0m sends leaves here, by the local address it
    /// names as its source.
    fn transmit(&self, transmit: Transmit) {
        let host = self
            .others
            .iter()
            .find(|(_, local)| *local == transmit.source)
            .map(|(socket, _)| &**socket)
            .or((transmit.source == self.local).then_some(&self.socket));
        if let Some(socket) = host {
            send(socket, transmit.destination, &transmit.contents, &self.link);
        } else if let Some((server, indication)) =
            self.relays
                .indication(transmit.source, transmit.destination, &transmit.contents)
        {
            send(&self.socket, server, &indication, &self.link);
        } else {
            tracing::debug!(link = self.link, source = %transmit.source, "no WebRTC socket or TURN allocation at that local address");
        }
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Connected => {
                self.connected = true;
                self.connect_by = None;
                self.shared.enter(Phase::Connected);
                tracing::info!(link = self.link, "WebRTC connected");
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) if self.connected => {
                self.end(Phase::Lost, "ICE disconnected");
            }
            Event::Closed => self.end(Phase::Lost, "the remote closed the connection"),
            Event::StreamPaused(paused) => self.shared.enter(Phase::Receiving {
                paused: paused.paused,
            }),
            Event::ChannelOpen(id, label) if label == CLOCK_CHANNEL => {
                self.clock = Some(id);
                if !self.media.is_send() {
                    self.probe_at = Some(Instant::now());
                }
            }
            Event::ChannelClose(id) if self.clock == Some(id) => {
                self.clock = None;
                self.probe_at = None;
            }
            Event::ChannelData(data) if self.clock == Some(data.id) => self.on_clock(&data),
            Event::MediaData(data) => {
                let Media::Receive { packets, .. } = &self.media else {
                    return;
                };
                self.shared
                    .meter
                    .record(data.data.len() as u64, host_time::now());
                self.shared.enter(Phase::Receiving { paused: false });
                let packet = IncomingPacket {
                    rtp_time: data.time.rebase(Frequency::FORTY_EIGHT_KHZ).numer(),
                    data: data.data,
                };
                if let Err(TrySendError::Full(_)) = packets.try_send(packet) {
                    tracing::debug!(link = self.link, "WebRTC decoder behind; dropped a packet");
                }
            }
            _ => {}
        }
    }

    /// Claude: every datagram the socket receives arrives here first.
    fn receive(&mut self, datagram: &[u8], from: SocketAddr) {
        if stun::transaction(datagram).is_some_and(|transaction| self.issued.contains(&transaction))
        {
            return;
        }
        match self.relays.receive(datagram, from, Instant::now()) {
            Received::Other => self.deliver(datagram, from, self.local),
            Received::Data {
                relayed,
                peer,
                payload,
            } => self.deliver(payload, peer, relayed),
            Received::Consumed => flush(&self.socket, &mut self.relays, &self.link),
        }
    }

    /// Claude: hands str0m a datagram from `from`, received at the local address `to`.
    fn deliver(&mut self, datagram: &[u8], from: SocketAddr, to: SocketAddr) {
        let Ok(contents) = datagram.try_into() else {
            tracing::debug!(link = self.link, %from, "dropped a datagram that is not WebRTC");
            return;
        };
        let now = Instant::now();
        let input = Input::Receive(
            now,
            Receive {
                proto: Protocol::Udp,
                source: from,
                destination: to,
                contents,
            },
        );
        if self.connected && self.rtc.accepts(&input) {
            let through_relay = !self.is_host(to) || self.relays.relays(from);
            let relayed = self.paths.record(now, through_relay);
            let mut state = self.shared.state();
            if state.peer != Some(from) || state.relayed != relayed {
                tracing::debug!(link = self.link, peer = %from, relayed, "WebRTC path");
            }
            state.peer = Some(from);
            state.relayed = relayed;
        }
        if let Err(e) = self.rtc.handle_input(input) {
            tracing::debug!(link = self.link, %from, error = %e, "str0m refused a datagram");
        }
    }

    fn is_host(&self, address: SocketAddr) -> bool {
        address == self.local || self.others.iter().any(|(_, local)| *local == address)
    }

    /// Claude: a sender answers each probe with its clock and latest capture;
    /// a receiver feeds each reply to its sender's clock.
    fn on_clock(&mut self, data: &ChannelData) {
        let arrived = now_nanos();
        match (ClockMessage::decode(&data.data), &self.media) {
            (Some(ClockMessage::Probe { sent }), Media::Send(_)) => {
                let reply = ClockMessage::Reply {
                    probe: sent,
                    received: arrived,
                    sent: now_nanos(),
                    latest: self.latest,
                };
                self.send_clock(&reply);
            }
            (Some(reply @ ClockMessage::Reply { .. }), Media::Receive { clock, .. }) => {
                clock.record(reply, arrived);
            }
            _ => tracing::debug!(link = self.link, "ignored a clock message"),
        }
    }

    fn send_clock(&mut self, message: &ClockMessage) {
        let Some(mut channel) = self.clock.and_then(|id| self.rtc.channel(id)) else {
            return;
        };
        if let Err(e) = channel.write(true, &message.encode()) {
            tracing::debug!(link = self.link, error = %e, "clock message not sent");
        }
    }

    fn probe(&mut self, now: Instant) {
        if self.probe_at.is_none_or(|at| now < at) {
            return;
        }
        self.send_clock(&ClockMessage::Probe { sent: now_nanos() });
        self.probes += 1;
        let interval = if self.probes < QUICK_PROBES {
            QUICK_PROBE_INTERVAL
        } else {
            PROBE_INTERVAL
        };
        self.probe_at = Some(now + interval);
    }

    fn tick(&mut self) {
        let now = Instant::now();
        self.probe(now);
        if self.connect_by.is_some_and(|by| now >= by) {
            self.end(
                Phase::Unreachable,
                "not connected within 10 s of the answer",
            );
            return;
        }
        self.relays.handle_timeout(now);
        flush(&self.socket, &mut self.relays, &self.link);
        if let Err(e) = self.rtc.handle_input(Input::Timeout(now)) {
            self.end(Phase::Lost, &format!("str0m failed: {e}"));
        }
    }

    fn permit(&mut self, peers: &[IpAddr]) {
        self.relays.permit(peers, Instant::now());
        flush(&self.socket, &mut self.relays, &self.link);
    }

    fn accept_answer(&mut self, Answered { answer, peers }: Answered) {
        self.answers = None;
        let Some(pending) = self.pending.take() else {
            return;
        };
        match self.rtc.sdp_api().accept_answer(pending, answer) {
            Ok(()) => {
                self.connect_by = Some(Instant::now() + CONNECT_DEADLINE);
                self.permit(&peers);
                tracing::info!(link = self.link, "WebRTC answer applied");
            }
            Err(e) => self.end(Phase::Lost, &format!("the answer was not accepted: {e}")),
        }
    }

    /// Claude: one packet per write, and only once connected: str0m would drop
    /// it anyway, and nothing is queued for later.
    fn write(&mut self, packet: OutgoingPacket) {
        if !self.connected {
            return;
        }
        let Some(writer) = self.rtc.writer(self.mid) else {
            return;
        };
        let pt = match self.pt {
            Some(pt) => pt,
            None => {
                let Some(params) = writer
                    .payload_params()
                    .find(|params| params.spec().codec == Codec::Opus)
                else {
                    tracing::warn!(link = self.link, "the remote negotiated no Opus");
                    return;
                };
                *self.pt.insert(params.pt())
            }
        };
        if let Some(captured) = packet.captured_nanos {
            self.latest = Some(Capture {
                rtp_time: packet.rtp_time,
                captured,
            });
        }
        let len = packet.data.len() as u64;
        let time = MediaTime::new(packet.rtp_time, Frequency::FORTY_EIGHT_KHZ);
        match writer.write(pt, packet.captured, time, packet.data) {
            Ok(()) => self.shared.meter.record(len, host_time::now()),
            Err(e) => tracing::debug!(link = self.link, error = %e, "WebRTC write failed"),
        }
    }

    fn end(&mut self, phase: Phase, reason: &str) {
        if self.shared.state().phase.is_final() {
            return;
        }
        self.shared.enter(phase);
        match phase {
            Phase::Unreachable => {
                tracing::warn!(link = self.link, reason, "WebRTC peer unreachable")
            }
            _ => tracing::info!(link = self.link, reason, "WebRTC peer lost"),
        }
    }

    /// Claude: tells the remote the connection is over and sends what that
    /// produces before the socket goes.
    async fn close(&mut self) {
        if let Err(e) = self.rtc.close() {
            tracing::debug!(link = self.link, error = %e, "WebRTC close failed");
        }
        for _ in 0..CLOSE_PASSES {
            if self.drain().is_err() || !self.rtc.is_alive() {
                break;
            }
            if self
                .rtc
                .handle_input(Input::Timeout(Instant::now()))
                .is_err()
            {
                break;
            }
        }
        self.release().await;
    }

    /// Claude: awaits the answers because a release can meet a stale nonce.
    async fn release(&mut self) {
        let deadline = Instant::now() + RELEASE_WAIT;
        self.relays.close(Instant::now());
        let mut buffer = [0u8; DATAGRAM_MAX];
        loop {
            let now = Instant::now();
            self.relays.handle_timeout(now);
            flush(&self.socket, &mut self.relays, &self.link);
            if self.relays.idle() || now >= deadline {
                break;
            }
            let wake = self.relays.timeout().map_or(deadline, |at| at.min(deadline));
            if let Ok(Ok((len, from))) = tokio::time::timeout_at(
                tokio::time::Instant::from_std(wake),
                self.socket.recv_from(&mut buffer),
            )
            .await
            {
                self.relays.receive(&buffer[..len], from, Instant::now());
            }
        }
    }
}

struct Arrival {
    datagram: Vec<u8>,
    from: SocketAddr,
    to: SocketAddr,
}

/// Claude: hands a session what one of its other sockets receives. A full
/// queue drops the datagram, as the network would.
async fn read(
    socket: Arc<UdpSocket>,
    local: SocketAddr,
    arrivals: mpsc::Sender<Arrival>,
    stop: CancellationToken,
) {
    let mut buffer = [0u8; DATAGRAM_MAX];
    loop {
        let received = tokio::select! {
            () = stop.cancelled() => return,
            received = socket.recv_from(&mut buffer) => received,
        };
        match received {
            Ok((len, from)) => {
                let arrival = Arrival {
                    datagram: buffer[..len].to_vec(),
                    from,
                    to: local,
                };
                if let Err(mpsc::error::TrySendError::Closed(_)) = arrivals.try_send(arrival) {
                    return;
                }
            }
            Err(e) => tracing::debug!(%local, error = %e, "WebRTC receive failed"),
        }
    }
}

async fn next<T>(receiver: &mut Option<mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}
