use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use super::*;
use crate::stun::{Attr, Class, Message, MessageBuilder, Method};
use crate::webrtc::RELAY_ONLY;

const REALM: &str = "spaghetti-test";
const USERNAME: &str = "1767225600:link";
const PASSWORD: &str = "c2VjcmV0";
const PERMISSION_LIFETIME: Duration = Duration::from_secs(300);

#[derive(Clone, Copy)]
struct Settings {
    lifetime: Duration,
    stale_nonces: bool,
    separate_relay: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            lifetime: Duration::from_secs(600),
            stale_nonces: false,
            separate_relay: false,
        }
    }
}

#[derive(Default)]
struct Seen {
    challenged: u32,
    stale: u32,
    refused: u32,
    allocated: Vec<(SocketAddr, Instant)>,
    refreshed: Vec<(SocketAddr, Instant)>,
    released: Vec<SocketAddr>,
    expired: Vec<SocketAddr>,
    permitted: Vec<(SocketAddr, IpAddr)>,
    relayed: u64,
    direct: HashMap<SocketAddr, u64>,
    indicated: HashMap<SocketAddr, u64>,
}

/// Claude: a single-port TURN relay as uTURN's README describes it: every
/// allocation gets the server's own address as its relayed address, and
/// client-to-client traffic is routed by the ICE ufrags in STUN USERNAME, then
/// by the pairing that sets up. Credentials, nonces, REQUESTED-TRANSPORT,
/// permissions and lifetimes are enforced.
struct FakeTurn {
    address: SocketAddr,
    relay: SocketAddr,
    seen: Arc<Mutex<Seen>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeTurn {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeTurn {
    async fn start(settings: Settings) -> Self {
        let control = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let relay = if settings.separate_relay {
            let lan = crate::webrtc::default_route_ipv4().unwrap();
            Arc::new(UdpSocket::bind((lan, 0)).await.unwrap())
        } else {
            Arc::clone(&control)
        };
        let seen = Arc::new(Mutex::new(Seen::default()));
        let address = control.local_addr().unwrap();
        let relay_address = relay.local_addr().unwrap();
        let mut server = Relay {
            settings,
            control,
            relay,
            relay_address,
            nonce: 1,
            allocations: HashMap::new(),
            ufrags: HashMap::new(),
            pairs: HashMap::new(),
            seen: Arc::clone(&seen),
        };
        let task = tokio::spawn(async move { server.run().await });
        Self {
            address,
            relay: relay_address,
            seen,
            task,
        }
    }

    fn server(&self, credential: &str) -> TurnServer {
        TurnServer {
            address: self.address.to_string(),
            username: USERNAME.into(),
            credential: credential.into(),
        }
    }

    fn seen(&self) -> std::sync::MutexGuard<'_, Seen> {
        self.seen.lock().unwrap()
    }
}

struct Allocation {
    expires: Instant,
    permissions: HashMap<IpAddr, Instant>,
}

struct Relay {
    settings: Settings,
    control: Arc<UdpSocket>,
    relay: Arc<UdpSocket>,
    relay_address: SocketAddr,
    nonce: u32,
    allocations: HashMap<SocketAddr, Allocation>,
    ufrags: HashMap<String, SocketAddr>,
    pairs: HashMap<SocketAddr, SocketAddr>,
    seen: Arc<Mutex<Seen>>,
}

fn ufrags(datagram: &[u8]) -> Option<(String, String)> {
    let message = Message::parse(datagram)?;
    if (message.method, message.class) != (Method::Binding, Class::Request) {
        return None;
    }
    let username = std::str::from_utf8(message.attribute(Attr::Username)?).ok()?;
    let (receiver, sender) = username.split_once(':')?;
    Some((receiver.to_string(), sender.to_string()))
}

impl Relay {
    async fn run(&mut self) {
        let (mut on_control, mut on_relay) = (vec![0u8; 2048], vec![0u8; 2048]);
        let separate = self.settings.separate_relay;
        loop {
            let (datagram, from, outside) = tokio::select! {
                Ok((len, from)) = self.control.recv_from(&mut on_control) => {
                    (on_control[..len].to_vec(), from, false)
                }
                Ok((len, from)) = self.relay.recv_from(&mut on_relay), if separate => {
                    (on_relay[..len].to_vec(), from, true)
                }
            };
            let now = Instant::now();
            let seen = Arc::clone(&self.seen);
            self.allocations.retain(|client, allocation| {
                let live = allocation.expires > now;
                if !live {
                    seen.lock().unwrap().expired.push(*client);
                }
                live
            });
            if outside {
                self.arrived(from, &datagram).await;
                continue;
            }
            match Message::parse(&datagram) {
                Some(message)
                    if message.class == Class::Request
                        && matches!(
                            message.method,
                            Method::Allocate | Method::Refresh | Method::CreatePermission
                        ) =>
                {
                    let reply = self.answer(from, &message, now);
                    let _ = self.control.send_to(&reply, from).await;
                }
                Some(message)
                    if (message.method, message.class) == (Method::Send, Class::Indication) =>
                {
                    self.send(from, &message, now).await
                }
                _ if !separate => self.arrived(from, &datagram).await,
                _ => {}
            }
        }
    }

    async fn arrived(&mut self, peer: SocketAddr, datagram: &[u8]) {
        *self.seen.lock().unwrap().direct.entry(peer).or_default() += 1;
        self.route(peer, datagram, peer).await;
    }

    fn nonce(&self) -> String {
        format!("nonce-{}", self.nonce)
    }

    fn refuse(&self, request: &Message, code: u16) -> Vec<u8> {
        let mut seen = self.seen.lock().unwrap();
        match code {
            401 => seen.challenged += 1,
            438 => seen.stale += 1,
            _ => seen.refused += 1,
        }
        MessageBuilder::new(request.method, Class::Error, request.transaction)
            .attribute(Attr::ErrorCode, &stun::tests::error_code(code))
            .attribute(Attr::Realm, REALM.as_bytes())
            .attribute(Attr::Nonce, self.nonce().as_bytes())
            .finish()
    }

    fn answer(&mut self, client: SocketAddr, request: &Message, now: Instant) -> Vec<u8> {
        if request.attribute(Attr::MessageIntegrity).is_none()
            || request.attribute(Attr::Username) != Some(USERNAME.as_bytes())
            || request.attribute(Attr::Realm) != Some(REALM.as_bytes())
        {
            return self.refuse(request, 401);
        }
        if request.attribute(Attr::Nonce) != Some(self.nonce().as_bytes()) {
            return self.refuse(request, 438);
        }
        let key = stun::long_term_key(USERNAME, REALM, PASSWORD);
        if request.integrity(&key) != Some(true) {
            return self.refuse(request, 401);
        }
        let success = MessageBuilder::new(request.method, Class::Success, request.transaction);
        let lifetime = self.settings.lifetime;
        let lifetime_value = u32::try_from(lifetime.as_secs()).unwrap().to_be_bytes();
        let reply = match request.method {
            Method::Allocate => {
                if request.attribute(Attr::RequestedTransport) != Some(&[17, 0, 0, 0][..]) {
                    return self.refuse(request, 400);
                }
                if self.allocations.contains_key(&client) {
                    return self.refuse(request, 437);
                }
                let (SocketAddr::V4(relayed), SocketAddr::V4(mapped)) =
                    (self.relay_address, client)
                else {
                    unreachable!("the fake server is IPv4")
                };
                self.allocations.insert(
                    client,
                    Allocation {
                        expires: now + lifetime,
                        permissions: HashMap::new(),
                    },
                );
                self.seen.lock().unwrap().allocated.push((client, now));
                success
                    .address(Attr::XorRelayedAddress, relayed)
                    .address(Attr::XorMappedAddress, mapped)
                    .attribute(Attr::Lifetime, &lifetime_value)
            }
            Method::Refresh => {
                let Some(allocation) = self.allocations.get_mut(&client) else {
                    return self.refuse(request, 437);
                };
                if request.lifetime() == Some(Duration::ZERO) {
                    self.allocations.remove(&client);
                    self.seen.lock().unwrap().released.push(client);
                    success.attribute(Attr::Lifetime, &0u32.to_be_bytes())
                } else {
                    allocation.expires = now + lifetime;
                    self.seen.lock().unwrap().refreshed.push((client, now));
                    success.attribute(Attr::Lifetime, &lifetime_value)
                }
            }
            _ => {
                let Some(peer) = request.address(Attr::XorPeerAddress) else {
                    return self.refuse(request, 400);
                };
                let Some(allocation) = self.allocations.get_mut(&client) else {
                    return self.refuse(request, 437);
                };
                let ip = IpAddr::V4(*peer.ip());
                allocation.permissions.insert(ip, now + PERMISSION_LIFETIME);
                self.seen.lock().unwrap().permitted.push((client, ip));
                success
            }
        };
        if self.settings.stale_nonces && request.method != Method::CreatePermission {
            self.nonce += 1;
        }
        reply.integrity(&key)
    }

    fn permits(&self, client: SocketAddr, ip: IpAddr, now: Instant) -> bool {
        self.allocations
            .get(&client)
            .and_then(|allocation| allocation.permissions.get(&ip))
            .is_some_and(|until| *until > now)
    }

    async fn send(&mut self, client: SocketAddr, indication: &Message<'_>, now: Instant) {
        if !self.allocations.contains_key(&client) {
            return;
        }
        *self
            .seen
            .lock()
            .unwrap()
            .indicated
            .entry(client)
            .or_default() += 1;
        let (Some(peer), Some(data)) = (
            indication.address(Attr::XorPeerAddress),
            indication.attribute(Attr::Data),
        ) else {
            return;
        };
        let peer = SocketAddr::V4(peer);
        if peer == self.relay_address {
            self.route(client, data, self.relay_address).await;
        } else if self.permits(client, peer.ip(), now) {
            let _ = self.relay.send_to(data, peer).await;
        }
    }

    async fn route(&mut self, sender: SocketAddr, payload: &[u8], seen_as: SocketAddr) {
        let now = Instant::now();
        let by_ufrag = ufrags(payload).and_then(|(receiver, own)| {
            self.ufrags.insert(own, sender);
            self.ufrags.get(&receiver).copied()
        });
        let allocated =
            |client: &SocketAddr| *client != sender && self.allocations.contains_key(client);
        let target = by_ufrag
            .filter(allocated)
            .or_else(|| self.pairs.get(&sender).copied().filter(allocated))
            .or_else(|| {
                self.allocations
                    .keys()
                    .copied()
                    .find(|client| *client != sender && self.permits(*client, seen_as.ip(), now))
            });
        let Some(target) = target else {
            return;
        };
        if !self.permits(target, seen_as.ip(), now) {
            return;
        }
        self.pairs.insert(sender, target);
        self.pairs.insert(target, sender);
        let SocketAddr::V4(seen_as) = seen_as else {
            return;
        };
        let data = MessageBuilder::new(Method::Data, Class::Indication, stun::transaction_id())
            .address(Attr::XorPeerAddress, seen_as)
            .attribute(Attr::Data, payload)
            .finish();
        let _ = self.control.send_to(&data, target).await;
        self.seen.lock().unwrap().relayed += 1;
    }
}

fn relay_only(role: PeerRole, link: &str, turn: &FakeTurn) -> WebRtcPeer {
    RELAY_ONLY.lock().unwrap().push(link.to_string());
    with_turn(role, link, turn, PASSWORD)
}

fn with_turn(role: PeerRole, link: &str, turn: &FakeTurn, credential: &str) -> WebRtcPeer {
    WebRtcPeer {
        turn: vec![turn.server(credential)],
        ..peer(role, link)
    }
}

fn candidates(sdp: &str) -> Vec<(SocketAddr, String)> {
    sdp.split("\r\n")
        .filter(|line| line.starts_with("a=candidate:"))
        .map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            let address = SocketAddr::new(fields[4].parse().unwrap(), fields[5].parse().unwrap());
            (address, fields[7].to_string())
        })
        .collect()
}

async fn link(
    daemon: &Daemon,
    offerer: WebRtcPeer,
    answerer: WebRtcPeer,
) -> (PeerRef, PeerRef, Arc<Ring>, Producer) {
    let (fixture, sinks) = daemon.fixture(48000.0);
    let producer = Producer::start(sinks, 48000.0);
    let source_id = daemon.create_source(answerer).await;
    let route = PeerRef::Route(daemon.create_route(fixture, offerer).await);
    let (ring, _) = daemon.sink(source_id);
    (route, PeerRef::Source(source_id), ring, producer)
}

#[tokio::test(flavor = "multi_thread")]
async fn two_relay_only_peers_carry_a_tone_through_one_shared_relayed_address() {
    let turn = FakeTurn::start(Settings::default()).await;
    let daemon = Daemon::new();
    let (route, source, ring, _producer) = link(
        &daemon,
        relay_only(PeerRole::Offer, "both-relay-offer", &turn),
        relay_only(PeerRole::Answer, "both-relay-answer", &turn),
    )
    .await;
    daemon.exchange(route, source, direct).await;
    let offer = daemon.description(route).await;
    let answer = daemon.description(source).await;
    for sdp in [&offer, &answer] {
        assert_eq!(
            candidates(sdp),
            [(turn.relay, "relay".to_string())],
            "{sdp}"
        );
    }

    for peer in [route, source] {
        daemon
            .wait_for(peer, FlowState::Flowing, Duration::from_secs(5))
            .await;
    }
    let pcm = collect(&ring, 2.0).await;
    assert_tone(&pcm, "relayed both ways");
    for peer in [route, source] {
        let status = daemon.status(peer).await;
        assert!(status.relayed, "{peer:?}: {status:?}");
        assert_eq!(status.peer, Some(turn.relay.to_string()), "{peer:?}");
    }
    let seen = turn.seen();
    assert_eq!(seen.allocated.len(), 2);
    for (client, _) in &seen.allocated {
        assert!(
            seen.permitted.contains(&(*client, turn.relay.ip())),
            "{client} permitted only {:?}",
            seen.permitted
        );
    }
    assert!(seen.relayed > 100, "relayed {} datagrams", seen.relayed);
}

/// Claude: the unrestricted end reaches the relayed address straight from its
/// host candidate, and must recognise what comes back from that address as
/// relayed by the address its own allocation was granted.
async fn one_end_relaying(settings: Settings, relaying: PeerRole, name: &str) {
    let turn = FakeTurn::start(settings).await;
    let daemon = Daemon::new();
    let spec = |role: PeerRole| {
        let link = format!("{name}-{role:?}");
        if role == relaying {
            relay_only(role, &link, &turn)
        } else {
            with_turn(role, &link, &turn, PASSWORD)
        }
    };
    let (route, source, ring, _producer) =
        link(&daemon, spec(PeerRole::Offer), spec(PeerRole::Answer)).await;
    daemon.exchange(route, source, direct).await;
    let (relaying_end, open_end) = match relaying {
        PeerRole::Offer => (route, source),
        PeerRole::Answer => (source, route),
    };
    let restricted = daemon.description(relaying_end).await;
    assert_eq!(
        candidates(&restricted),
        [(turn.relay, "relay".to_string())],
        "{restricted}"
    );
    let open = daemon.description(open_end).await;
    let open_host = host_candidate(&open);
    assert!(open.contains("typ relay"), "{open}");

    for peer in [route, source] {
        daemon
            .wait_for(peer, FlowState::Flowing, Duration::from_secs(5))
            .await;
    }
    let pcm = collect(&ring, 2.0).await;
    assert_tone(&pcm, name);
    for peer in [route, source] {
        let status = daemon.status(peer).await;
        assert!(status.relayed, "{name}, {peer:?}: {status:?}");
    }
    assert_eq!(
        daemon.status(open_end).await.peer,
        Some(turn.relay.to_string()),
        "{name}"
    );
    let seen = turn.seen();
    let straight = seen.direct.get(&open_host).copied().unwrap_or(0);
    let indicated = seen.indicated.get(&open_host).copied().unwrap_or(0);
    assert!(
        straight > indicated,
        "{name}: the unrestricted end sent {straight} datagrams straight to the relayed address and {indicated} through its allocation"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn either_end_relaying_alone_makes_both_ends_report_relayed() {
    one_end_relaying(Settings::default(), PeerRole::Offer, "mixed-offer").await;
    let elsewhere = Settings {
        separate_relay: true,
        ..Settings::default()
    };
    one_end_relaying(elsewhere, PeerRole::Answer, "mixed-answer").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_direct_path_wins_over_the_relay_and_reports_direct() {
    let turn = FakeTurn::start(Settings::default()).await;
    let daemon = Daemon::new();
    let (route, source, ring, _producer) = link(
        &daemon,
        with_turn(PeerRole::Offer, "direct-offer", &turn, PASSWORD),
        with_turn(PeerRole::Answer, "direct-answer", &turn, PASSWORD),
    )
    .await;
    daemon.exchange(route, source, direct).await;
    let offer = daemon.description(route).await;
    let answer = daemon.description(source).await;
    for peer in [route, source] {
        daemon
            .wait_for(peer, FlowState::Flowing, Duration::from_secs(5))
            .await;
    }
    let pcm = collect(&ring, 2.0).await;
    assert_tone(&pcm, "direct with TURN offered");
    for (peer, remote) in [(route, &answer), (source, &offer)] {
        let status = daemon.status(peer).await;
        assert!(!status.relayed, "{peer:?}: {status:?}");
        assert_eq!(status.peer, Some(host_candidate(remote).to_string()));
    }

    let seen = turn.seen();
    for (local, remote) in [(&offer, &answer), (&answer, &offer)] {
        let client = host_candidate(local);
        assert!(
            seen.allocated
                .iter()
                .any(|(allocated, _)| *allocated == client),
            "{client} has no allocation"
        );
        for (address, kind) in candidates(remote) {
            assert!(
                seen.permitted.contains(&(client, address.ip())),
                "{client} did not permit the remote's {kind} candidate {address}: {:?}",
                seen.permitted
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_turn_credential_offers_no_relayed_candidate_and_connects_directly() {
    let turn = FakeTurn::start(Settings::default()).await;
    let daemon = Daemon::new();
    let created = Instant::now();
    let (route, source, ring, _producer) = link(
        &daemon,
        with_turn(PeerRole::Offer, "refused-offer", &turn, "not the secret"),
        peer(PeerRole::Answer, "refused-answer"),
    )
    .await;
    let offer = daemon.description(route).await;
    assert!(
        created.elapsed() < Duration::from_millis(1500),
        "the refused allocation held the offer back {:?}",
        created.elapsed()
    );
    assert!(
        offer.contains("typ host") && !offer.contains("typ relay"),
        "{offer}"
    );
    {
        let seen = turn.seen();
        assert!(seen.allocated.is_empty());
        assert_eq!(
            seen.challenged, 2,
            "the challenge and the refused credential, and no retry after"
        );
    }

    daemon.exchange(route, source, direct).await;
    for peer in [route, source] {
        daemon
            .wait_for(peer, FlowState::Flowing, Duration::from_secs(5))
            .await;
    }
    let pcm = collect(&ring, 2.0).await;
    assert_tone(&pcm, "direct after a refused credential");
    for peer in [route, source] {
        assert!(!daemon.status(peer).await.relayed, "{peer:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn allocations_are_refreshed_before_their_lifetime_ends_and_released_on_close() {
    let lifetime = Duration::from_secs(3);
    let turn = FakeTurn::start(Settings {
        lifetime,
        stale_nonces: true,
        ..Settings::default()
    })
    .await;
    let daemon = Daemon::new();
    let (route, source, ring, _producer) = link(
        &daemon,
        relay_only(PeerRole::Offer, "refresh-offer", &turn),
        relay_only(PeerRole::Answer, "refresh-answer", &turn),
    )
    .await;
    daemon.exchange(route, source, direct).await;
    for peer in [route, source] {
        daemon
            .wait_for(peer, FlowState::Flowing, Duration::from_secs(5))
            .await;
    }
    let pcm = collect(&ring, 7.0).await;
    assert_tone(&pcm, "relayed past two lifetimes");
    for peer in [route, source] {
        assert!(daemon.status(peer).await.relayed, "{peer:?}");
    }
    {
        let now = Instant::now();
        let seen = turn.seen();
        assert!(seen.expired.is_empty(), "expired: {:?}", seen.expired);
        assert!(seen.stale > 0, "no nonce went stale");
        assert_eq!(seen.allocated.len(), 2);
        for &(client, allocated) in &seen.allocated {
            let renewals: Vec<Instant> = std::iter::once(allocated)
                .chain(
                    seen.refreshed
                        .iter()
                        .filter(|(refreshed, _)| *refreshed == client)
                        .map(|(_, at)| *at),
                )
                .chain([now])
                .collect();
            assert!(
                renewals.len() >= 6,
                "{client} refreshed {} times",
                renewals.len() - 2
            );
            for pair in renewals.windows(2) {
                assert!(
                    pair[1] - pair[0] < lifetime,
                    "{client} went {:?} without a refresh",
                    pair[1] - pair[0]
                );
            }
        }
    }

    let PeerRef::Route(route_id) = route else {
        unreachable!()
    };
    let PeerRef::Source(source_id) = source else {
        unreachable!()
    };
    for request in [
        Request::DestroyRoute { route_id },
        Request::DestroySource { source_id },
    ] {
        let response = daemon.request(request).await;
        assert!(matches!(response, Response::Ok), "{response:?}");
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let seen = turn.seen();
    for (client, _) in &seen.allocated {
        assert!(seen.released.contains(client), "{client} was not released");
    }
}

/// Claude: run with `--ignored` and SPAGHETTI_TEST_TURN set to
/// host:port,username,credential.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a TURN server in SPAGHETTI_TEST_TURN"]
async fn two_relay_only_peers_carry_a_tone_through_a_real_turn_server() {
    let Ok(server) = std::env::var("SPAGHETTI_TEST_TURN") else {
        eprintln!("SPAGHETTI_TEST_TURN is not set; nothing to test against");
        return;
    };
    let mut parts = server.splitn(3, ',');
    let (Some(address), Some(username), Some(credential)) =
        (parts.next(), parts.next(), parts.next())
    else {
        panic!("SPAGHETTI_TEST_TURN is not host:port,username,credential");
    };
    let server = TurnServer {
        address: address.into(),
        username: username.into(),
        credential: credential.into(),
    };
    let spec = |role: PeerRole, link: &str| {
        RELAY_ONLY.lock().unwrap().push(link.to_string());
        WebRtcPeer {
            turn: vec![server.clone()],
            ..peer(role, link)
        }
    };
    let daemon = Daemon::new();
    let (route, source, ring, _producer) = link(
        &daemon,
        spec(PeerRole::Offer, "real-turn-offer"),
        spec(PeerRole::Answer, "real-turn-answer"),
    )
    .await;
    daemon.exchange(route, source, direct).await;
    for peer in [route, source] {
        let sdp = daemon.description(peer).await;
        let kinds: Vec<String> = candidates(&sdp).into_iter().map(|(_, kind)| kind).collect();
        assert_eq!(kinds, ["relay"], "{peer:?} was allocated nothing: {sdp}");
    }
    for peer in [route, source] {
        daemon
            .wait_for(peer, FlowState::Flowing, Duration::from_secs(10))
            .await;
    }
    let pcm = collect(&ring, 3.0).await;
    assert_tone(&pcm, "through the TURN server");
    for peer in [route, source] {
        let status = daemon.status(peer).await;
        assert!(status.relayed, "{peer:?}: {status:?}");
    }
}
