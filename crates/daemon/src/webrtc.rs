use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use coreaudio_sys::AudioGetCurrentHostTime;
use spaghetti_common::ipc_protocol::{FlowState, FlowStatus, PeerRole, WebRtcPeer};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::format::{Codec, FormatParams};
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive, Transmit};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig, RtcError};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::flow::FlowMeter;
use crate::stun;

const STUN_DEADLINE: Duration = Duration::from_secs(1);
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
/// Claude: how long str0m holds the packets behind a lost one. Its default, a
/// second over 15 packets, stalls audio up to 280 ms after every loss.
pub const REORDER_TIMEOUT: Duration = Duration::from_millis(40);
const OPUS_PT: u8 = 111;
const DATAGRAM_MAX: usize = 2048;
const CLOSE_PASSES: usize = 4;

static CRYPTO: Once = Once::new();

/// Claude: one Opus packet on its way out: its RTP time at 48 kHz and when its
/// audio was captured.
pub struct OutgoingPacket {
    pub rtp_time: u64,
    pub captured: Instant,
    pub data: Vec<u8>,
}

/// Claude: one Opus packet as str0m released it, with its RTP time at 48 kHz.
pub struct IncomingPacket {
    pub rtp_time: u64,
    pub data: Arc<[u8]>,
}

/// Claude: the media a link carries, and which way.
pub enum Media {
    Send(mpsc::Receiver<OutgoingPacket>),
    Receive(SyncSender<IncomingPacket>),
}

impl Media {
    fn is_send(&self) -> bool {
        matches!(self, Self::Send(_))
    }

    /// Claude: the next packet to send; never resolves for a receiver.
    async fn outgoing(&mut self) -> Option<OutgoingPacket> {
        match self {
            Self::Send(packets) => packets.recv().await,
            Self::Receive(_) => std::future::pending().await,
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
}

/// Claude: how the controller's remote description reaches the session: an
/// offerer takes an answer, an answerer an offer.
enum Signal {
    Answer(mpsc::UnboundedSender<SdpAnswer>),
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
        if !spec.turn.is_empty() {
            tracing::warn!(
                link = spec.link,
                servers = spec.turn.len(),
                "TURN relays are not supported yet; offering no relayed candidate"
            );
        }
        let socket = std::net::UdpSocket::bind((default_route_ipv4()?, 0))
            .and_then(|socket| socket.set_nonblocking(true).map(|()| socket))
            .map_err(|e| format!("bind a WebRTC socket: {e}"))?;
        let local = socket
            .local_addr()
            .map_err(|e| format!("WebRTC socket address: {e}"))?;
        let host = Candidate::host(local, "udp")
            .map_err(|e| format!("{local} cannot be a WebRTC candidate: {e}"))?;
        let socket =
            UdpSocket::from_std(socket).map_err(|e| format!("register a WebRTC socket: {e}"))?;

        let shared = Arc::new(Shared {
            meter: FlowMeter::new(),
            state: Mutex::new(State {
                phase: Phase::Connecting,
                peer: None,
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
            host,
            stun: spec.stun.clone(),
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
                answers.send(answer).is_ok()
            }
            Signal::Offer(offers) => {
                let offer = SdpOffer::from_sdp_string(&sdp)
                    .map_err(|e| format!("the offer does not parse: {e}"))?;
                let lines = offer.media_lines.iter();
                let mid = audio_mid(
                    lines.map(|line| (line.typ.to_string(), line.direction(), line.mid())),
                    self.sending,
                )?;
                offers.send(Offered { offer, mid }).is_ok()
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

/// Claude: the address this host reaches the internet from. Connecting a UDP
/// socket only picks the route; nothing is sent.
fn default_route_ipv4() -> Result<Ipv4Addr, String> {
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
}

/// Claude: asks every STUN server, from the media socket itself, how the
/// internet sees it. Ends when all have answered or `STUN_DEADLINE` passed.
async fn gather(socket: &UdpSocket, servers: &[String], link: &str) -> Gathered {
    let mut gathered = Gathered {
        issued: HashSet::new(),
        mapped: Vec::new(),
    };
    if servers.is_empty() {
        return gathered;
    }
    let deadline = tokio::time::Instant::now() + STUN_DEADLINE;
    let resolved = tokio::time::timeout_at(
        deadline,
        futures::future::join_all(servers.iter().map(|server| resolve(server))),
    )
    .await
    .unwrap_or_default();
    let mut waiting = Vec::new();
    for (server, address) in servers.iter().zip(resolved) {
        let Some(address) = address else {
            tracing::warn!(link, server, "cannot resolve STUN server");
            continue;
        };
        let transaction = stun::transaction_id();
        match socket
            .send_to(&stun::binding_request(transaction), address)
            .await
        {
            Ok(_) => {
                gathered.issued.insert(transaction);
                waiting.push(transaction);
            }
            Err(e) => tracing::warn!(link, server, error = %e, "STUN request failed"),
        }
    }
    let mut buffer = [0u8; DATAGRAM_MAX];
    while !waiting.is_empty() {
        let Ok(received) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await
        else {
            break;
        };
        let Ok((len, _)) = received else { continue };
        let response = &buffer[..len];
        let Some(transaction) = stun::transaction(response) else {
            continue;
        };
        let Some(index) = waiting.iter().position(|t| *t == transaction) else {
            continue;
        };
        let Some(mapped) = stun::mapped_address(response, transaction) else {
            continue;
        };
        waiting.swap_remove(index);
        if !gathered.mapped.contains(&mapped) {
            gathered.mapped.push(mapped);
        }
    }
    if !waiting.is_empty() {
        tracing::info!(
            link,
            unanswered = waiting.len(),
            "STUN servers did not answer in time"
        );
    }
    gathered
}

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
    host: Candidate,
    stun: Vec<String>,
    media: Media,
    shared: Arc<Shared>,
    closing: CancellationToken,
}

impl Gathering {
    async fn offer(self, answers: mpsc::UnboundedReceiver<SdpAnswer>) {
        let gathered = gather(&self.socket, &self.stun, &self.link).await;
        let mut rtc = self.rtc(&gathered.mapped);
        let direction = if self.media.is_send() {
            Direction::SendOnly
        } else {
            Direction::RecvOnly
        };
        let mut changes = rtc.sdp_api();
        let mid = changes.add_media(MediaKind::Audio, direction, None, None, None);
        let (offer, pending) = changes.apply().expect("new media needs an offer");
        let mut session = self.into_session(rtc, gathered.issued, mid);
        session.pending = Some(pending);
        session.answers = Some(answers);
        session.publish(offer.to_sdp_string());
        session.run().await;
    }

    async fn answer(mut self, mut offers: mpsc::UnboundedReceiver<Offered>) {
        let mut buffer = [0u8; DATAGRAM_MAX];
        let Offered { offer, mid } = loop {
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
        let gathered = gather(&self.socket, &self.stun, &self.link).await;
        let mut rtc = self.rtc(&gathered.mapped);
        let accepted = rtc.sdp_api().accept_offer(offer);
        let mut session = self.into_session(rtc, gathered.issued, mid);
        match accepted {
            Ok(answer) => {
                session.publish(answer.to_sdp_string());
                session.run().await;
            }
            Err(e) => {
                session.end(Phase::Lost, &format!("the offer was not accepted: {e}"));
                session.close();
            }
        }
    }

    fn rtc(&self, mapped: &[SocketAddrV4]) -> Rtc {
        let mut rtc = rtc_config().build(Instant::now());
        rtc.add_local_candidate(self.host.clone());
        for &address in mapped {
            let address = SocketAddr::V4(address);
            if address == self.local {
                continue;
            }
            match Candidate::server_reflexive(address, self.local, "udp") {
                Ok(candidate) => {
                    rtc.add_local_candidate(candidate);
                }
                Err(e) => {
                    tracing::debug!(link = self.link, %address, error = %e, "skipped a reflexive candidate")
                }
            }
        }
        rtc
    }

    fn into_session(self, rtc: Rtc, issued: HashSet<stun::Transaction>, mid: Mid) -> Session {
        Session {
            link: self.link,
            rtc,
            socket: self.socket,
            local: self.local,
            issued,
            mid,
            media: self.media,
            shared: self.shared,
            closing: self.closing,
            pending: None,
            answers: None,
            connect_by: None,
            connected: false,
            peer: None,
            pt: None,
        }
    }
}

/// Claude: a running str0m session. The task that owns it is the only one
/// touching the `Rtc` and the socket.
struct Session {
    link: String,
    rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    /// Claude: our own STUN transactions; their replies never reach str0m.
    issued: HashSet<stun::Transaction>,
    mid: Mid,
    media: Media,
    shared: Arc<Shared>,
    closing: CancellationToken,
    /// Claude: an offerer's offer until its answer arrives on `answers`.
    pending: Option<SdpPendingOffer>,
    answers: Option<mpsc::UnboundedReceiver<SdpAnswer>>,
    /// Claude: an offerer that applied its answer must connect by then.
    connect_by: Option<Instant>,
    connected: bool,
    peer: Option<SocketAddr>,
    pt: Option<Pt>,
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
            let wake = self.connect_by.map_or(timeout, |by| by.min(timeout));
            tokio::select! {
                () = self.closing.cancelled() => break,
                Some(answer) = next(&mut self.answers) => self.accept_answer(answer),
                Some(packet) = self.media.outgoing() => self.write(packet),
                received = self.socket.recv_from(&mut buffer) => match received {
                    Ok((len, from)) => self.receive(&buffer[..len], from),
                    Err(e) => tracing::debug!(link = self.link, error = %e, "WebRTC receive failed"),
                },
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => self.tick(),
            }
        }
        self.close();
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
        if transmit.source != self.local {
            tracing::debug!(link = self.link, source = %transmit.source, "no WebRTC socket at that local address");
            return;
        }
        if let Err(e) = self
            .socket
            .try_send_to(&transmit.contents, transmit.destination)
        {
            tracing::debug!(link = self.link, destination = %transmit.destination, error = %e, "WebRTC send failed");
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
            Event::MediaData(data) => {
                let Media::Receive(packets) = &self.media else {
                    return;
                };
                self.shared
                    .meter
                    .record(data.data.len() as u64, unsafe { AudioGetCurrentHostTime() });
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
        self.deliver(datagram, from, self.local);
    }

    /// Claude: hands str0m a datagram from `from`, received at the local address `to`.
    fn deliver(&mut self, datagram: &[u8], from: SocketAddr, to: SocketAddr) {
        let Ok(contents) = datagram.try_into() else {
            tracing::debug!(link = self.link, %from, "dropped a datagram that is not WebRTC");
            return;
        };
        let input = Input::Receive(
            Instant::now(),
            Receive {
                proto: Protocol::Udp,
                source: from,
                destination: to,
                contents,
            },
        );
        if self.connected && self.peer != Some(from) && self.rtc.accepts(&input) {
            self.peer = Some(from);
            self.shared.state().peer = Some(from);
            tracing::debug!(link = self.link, peer = %from, "WebRTC peer address");
        }
        if let Err(e) = self.rtc.handle_input(input) {
            tracing::debug!(link = self.link, %from, error = %e, "str0m refused a datagram");
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if self.connect_by.is_some_and(|by| now >= by) {
            self.end(
                Phase::Unreachable,
                "not connected within 10 s of the answer",
            );
            return;
        }
        if let Err(e) = self.rtc.handle_input(Input::Timeout(now)) {
            self.end(Phase::Lost, &format!("str0m failed: {e}"));
        }
    }

    fn accept_answer(&mut self, answer: SdpAnswer) {
        self.answers = None;
        let Some(pending) = self.pending.take() else {
            return;
        };
        match self.rtc.sdp_api().accept_answer(pending, answer) {
            Ok(()) => {
                self.connect_by = Some(Instant::now() + CONNECT_DEADLINE);
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
        let len = packet.data.len() as u64;
        let time = MediaTime::new(packet.rtp_time, Frequency::FORTY_EIGHT_KHZ);
        match writer.write(pt, packet.captured, time, packet.data) {
            Ok(()) => self
                .shared
                .meter
                .record(len, unsafe { AudioGetCurrentHostTime() }),
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
    fn close(&mut self) {
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
    }
}

async fn next<T>(receiver: &mut Option<mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}
