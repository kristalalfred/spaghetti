use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use spaghetti_common::ipc_protocol::TurnServer;

use crate::stun::{self, Attr, Class, Message, MessageBuilder, Method, Transaction};

const UDP: u8 = 17;
/// Claude: permissions last 300 s; a minute early leaves room for retransmissions.
const PERMISSION_REFRESH: Duration = Duration::from_secs(240);
const FIRST_RTO: Duration = Duration::from_millis(250);
const MAX_RTO: Duration = Duration::from_secs(3);
const TRANSMISSIONS: u32 = 7;
/// Claude: the unsigned request, the answer to its challenge, and one stale nonce.
const ATTEMPTS: u8 = 3;

pub struct Relays {
    link: String,
    servers: Vec<Server>,
    pending: HashMap<Transaction, Request>,
    outgoing: VecDeque<(SocketAddr, Vec<u8>)>,
}

struct Server {
    name: String,
    address: SocketAddr,
    username: String,
    password: String,
    auth: Option<Auth>,
    /// Claude: kept after the allocation ends; a single-port relay grants every
    /// allocation this address.
    relayed: Option<SocketAddrV4>,
    allocation: Allocation,
    permissions: HashMap<Ipv4Addr, Option<Instant>>,
}

struct Auth {
    realm: String,
    nonce: Vec<u8>,
    key: [u8; 16],
}

enum Allocation {
    Requested,
    Live {
        expires: Instant,
        refresh_at: Option<Instant>,
    },
    Ended,
}

impl Allocation {
    fn granted(lifetime: Duration, now: Instant) -> Self {
        Self::Live {
            expires: now + lifetime,
            refresh_at: Some(now + lifetime / 2),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Allocate,
    Refresh,
    Permit(Ipv4Addr),
    Release,
}

struct Request {
    server: usize,
    kind: Kind,
    attempt: u8,
    nonce: Option<Vec<u8>>,
    datagram: Vec<u8>,
    sent: u32,
    resend_at: Instant,
}

pub enum Received<'a> {
    Consumed,
    Data {
        relayed: SocketAddr,
        peer: SocketAddr,
        payload: &'a [u8],
    },
    Other,
}

impl Server {
    fn live_relayed(&self) -> Option<SocketAddrV4> {
        match self.allocation {
            Allocation::Live { .. } => self.relayed,
            Allocation::Requested | Allocation::Ended => None,
        }
    }

    fn seal(&self, message: MessageBuilder) -> Vec<u8> {
        match &self.auth {
            Some(auth) => message
                .attribute(Attr::Username, self.username.as_bytes())
                .attribute(Attr::Realm, auth.realm.as_bytes())
                .attribute(Attr::Nonce, &auth.nonce)
                .integrity(&auth.key),
            None => message.finish(),
        }
    }

    fn learn(&mut self, challenge: &Message) -> bool {
        let Some(nonce) = challenge.attribute(Attr::Nonce) else {
            return false;
        };
        let realm = match challenge.attribute(Attr::Realm).map(std::str::from_utf8) {
            Some(Ok(realm)) => realm.to_string(),
            Some(Err(_)) => return false,
            None => match &self.auth {
                Some(auth) => auth.realm.clone(),
                None => return false,
            },
        };
        let key = stun::long_term_key(&self.username, &realm, &self.password);
        self.auth = Some(Auth {
            realm,
            nonce: nonce.to_vec(),
            key,
        });
        true
    }
}

fn rto(sent: u32) -> Duration {
    FIRST_RTO
        .saturating_mul(1 << (sent - 1).min(8))
        .min(MAX_RTO)
}

impl Relays {
    pub fn new(link: &str, servers: Vec<(SocketAddr, &TurnServer)>, now: Instant) -> Self {
        let mut relays = Self {
            link: link.to_string(),
            servers: servers
                .into_iter()
                .map(|(address, server)| Server {
                    name: server.address.clone(),
                    address,
                    username: server.username.clone(),
                    password: server.credential.clone(),
                    auth: None,
                    relayed: None,
                    allocation: Allocation::Requested,
                    permissions: HashMap::new(),
                })
                .collect(),
            pending: HashMap::new(),
            outgoing: VecDeque::new(),
        };
        for index in 0..relays.servers.len() {
            relays.ask(index, Kind::Allocate, 1, now);
        }
        relays
    }

    pub fn settled(&self) -> bool {
        !self
            .servers
            .iter()
            .any(|server| matches!(server.allocation, Allocation::Requested))
    }

    pub fn relayed(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.servers
            .iter()
            .filter_map(Server::live_relayed)
            .map(SocketAddr::V4)
    }

    /// Claude: the granted relayed address counts too: a single-port relay
    /// shares it among its allocations, so the remote's relay sends from it.
    pub fn relays(&self, address: SocketAddr) -> bool {
        self.servers.iter().any(|server| {
            server.address == address || server.relayed.map(SocketAddr::V4) == Some(address)
        })
    }

    pub fn abandon_unanswered(&mut self) {
        self.pending
            .retain(|_, request| !matches!(request.kind, Kind::Allocate));
        for server in &mut self.servers {
            if matches!(server.allocation, Allocation::Requested) {
                tracing::info!(
                    link = self.link,
                    server = server.name,
                    "TURN server did not answer in time; no relayed candidate"
                );
                server.allocation = Allocation::Ended;
            }
        }
    }

    pub fn poll_transmit(&mut self) -> Option<(SocketAddr, Vec<u8>)> {
        self.outgoing.pop_front()
    }

    pub fn timeout(&self) -> Option<Instant> {
        let mut at = self.pending.values().map(|request| request.resend_at).min();
        for server in &self.servers {
            if let Allocation::Live {
                expires,
                refresh_at,
            } = server.allocation
            {
                let permissions = server.permissions.values().flatten().copied();
                at = [at, Some(expires), refresh_at]
                    .into_iter()
                    .flatten()
                    .chain(permissions)
                    .min();
            }
        }
        at
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        let due: Vec<Transaction> = self
            .pending
            .iter()
            .filter(|(_, request)| request.resend_at <= now)
            .map(|(transaction, _)| *transaction)
            .collect();
        for transaction in due {
            let Some(request) = self.pending.get_mut(&transaction) else {
                continue;
            };
            if request.sent < TRANSMISSIONS {
                request.sent += 1;
                request.resend_at = now + rto(request.sent);
                let server = self.servers[request.server].address;
                self.outgoing.push_back((server, request.datagram.clone()));
            } else if let Some(request) = self.pending.remove(&transaction) {
                self.failed(request, None);
            }
        }
        for index in 0..self.servers.len() {
            let server = &mut self.servers[index];
            let Allocation::Live {
                expires,
                refresh_at,
            } = &mut server.allocation
            else {
                continue;
            };
            if now >= *expires {
                tracing::warn!(
                    link = self.link,
                    server = server.name,
                    "TURN allocation expired; its relayed candidate is dead"
                );
                server.allocation = Allocation::Ended;
                continue;
            }
            let refresh = refresh_at.is_some_and(|at| now >= at);
            if refresh {
                *refresh_at = None;
            }
            let mut due = Vec::new();
            for (ip, at) in &mut server.permissions {
                if at.is_some_and(|at| now >= at) {
                    *at = None;
                    due.push(*ip);
                }
            }
            if refresh {
                self.ask(index, Kind::Refresh, 1, now);
            }
            for ip in due {
                self.ask(index, Kind::Permit(ip), 1, now);
            }
        }
    }

    pub fn permit(&mut self, addresses: &[IpAddr], now: Instant) {
        for index in 0..self.servers.len() {
            if self.servers[index].live_relayed().is_none() {
                continue;
            }
            for address in addresses {
                let IpAddr::V4(ip) = *address else {
                    continue;
                };
                if self.servers[index].permissions.contains_key(&ip) {
                    continue;
                }
                self.servers[index].permissions.insert(ip, None);
                self.ask(index, Kind::Permit(ip), 1, now);
            }
        }
    }

    pub fn indication(
        &self,
        source: SocketAddr,
        peer: SocketAddr,
        payload: &[u8],
    ) -> Option<(SocketAddr, Vec<u8>)> {
        let SocketAddr::V4(peer) = peer else {
            return None;
        };
        let server = self
            .servers
            .iter()
            .find(|server| server.live_relayed().map(SocketAddr::V4) == Some(source))?;
        let datagram = MessageBuilder::new(Method::Send, Class::Indication, stun::transaction_id())
            .address(Attr::XorPeerAddress, peer)
            .attribute(Attr::Data, payload)
            .finish();
        Some((server.address, datagram))
    }

    pub fn receive<'d>(
        &mut self,
        datagram: &'d [u8],
        from: SocketAddr,
        now: Instant,
    ) -> Received<'d> {
        if !self.servers.iter().any(|server| server.address == from) {
            return Received::Other;
        }
        // Claude: ChannelData (RFC 7983); this end binds no channels.
        if datagram
            .first()
            .is_some_and(|byte| (0x40..=0x4F).contains(byte))
        {
            return Received::Consumed;
        }
        let Some(message) = Message::parse(datagram) else {
            return Received::Other;
        };
        match (message.method, message.class) {
            (Method::Binding, _) => Received::Other,
            (Method::Data, Class::Indication) => {
                let relayed = self
                    .servers
                    .iter()
                    .filter(|server| server.address == from)
                    .find_map(Server::live_relayed);
                match (
                    relayed,
                    message.address(Attr::XorPeerAddress),
                    message.attribute(Attr::Data),
                ) {
                    (Some(relayed), Some(peer), Some(payload)) => Received::Data {
                        relayed: relayed.into(),
                        peer: peer.into(),
                        payload,
                    },
                    _ => Received::Consumed,
                }
            }
            (_, Class::Success | Class::Error) => {
                self.answered(&message, now);
                Received::Consumed
            }
            _ => Received::Consumed,
        }
    }

    pub fn close(&mut self, now: Instant) {
        self.pending.clear();
        for index in 0..self.servers.len() {
            if self.servers[index].live_relayed().is_some() {
                self.servers[index].allocation = Allocation::Ended;
                self.ask(index, Kind::Release, 1, now);
            }
        }
    }

    pub fn idle(&self) -> bool {
        self.pending.is_empty()
    }

    fn ask(&mut self, index: usize, kind: Kind, attempt: u8, now: Instant) {
        let server = &self.servers[index];
        let transaction = stun::transaction_id();
        let request = match kind {
            Kind::Allocate => MessageBuilder::new(Method::Allocate, Class::Request, transaction)
                .attribute(Attr::RequestedTransport, &[UDP, 0, 0, 0]),
            Kind::Refresh => MessageBuilder::new(Method::Refresh, Class::Request, transaction),
            Kind::Permit(ip) => {
                MessageBuilder::new(Method::CreatePermission, Class::Request, transaction)
                    .address(Attr::XorPeerAddress, SocketAddrV4::new(ip, 0))
            }
            Kind::Release => MessageBuilder::new(Method::Refresh, Class::Request, transaction)
                .attribute(Attr::Lifetime, &0u32.to_be_bytes()),
        };
        let datagram = server.seal(request);
        self.outgoing.push_back((server.address, datagram.clone()));
        self.pending.insert(
            transaction,
            Request {
                server: index,
                kind,
                attempt,
                nonce: server.auth.as_ref().map(|auth| auth.nonce.clone()),
                datagram,
                sent: 1,
                resend_at: now + FIRST_RTO,
            },
        );
    }

    fn answered(&mut self, message: &Message, now: Instant) {
        let Some(request) = self.pending.get(&message.transaction) else {
            return;
        };
        let server = &self.servers[request.server];
        if message.class == Class::Success
            && request.nonce.is_some()
            && let Some(auth) = &server.auth
            && message.integrity(&auth.key) == Some(false)
        {
            tracing::debug!(
                link = self.link,
                server = server.name,
                "dropped a TURN response that failed its integrity check"
            );
            return;
        }
        let Some(request) = self.pending.remove(&message.transaction) else {
            return;
        };
        match message.class {
            Class::Success => self.succeeded(request, message, now),
            Class::Error | Class::Request | Class::Indication => {
                self.refused(request, message, now)
            }
        }
    }

    fn succeeded(&mut self, request: Request, message: &Message, now: Instant) {
        let server = &mut self.servers[request.server];
        match request.kind {
            Kind::Allocate => {
                match (message.address(Attr::XorRelayedAddress), message.lifetime()) {
                    (Some(relayed), Some(lifetime)) if !lifetime.is_zero() => {
                        tracing::info!(
                            link = self.link,
                            server = server.name,
                            %relayed,
                            lifetime = lifetime.as_secs(),
                            "TURN allocation granted"
                        );
                        server.relayed = Some(relayed);
                        server.allocation = Allocation::granted(lifetime, now);
                    }
                    _ => {
                        tracing::info!(
                            link = self.link,
                            server = server.name,
                            "TURN allocation granted no relayed address or lifetime; no relayed candidate"
                        );
                        server.allocation = Allocation::Ended;
                    }
                }
            }
            Kind::Refresh => {
                if !matches!(server.allocation, Allocation::Live { .. }) {
                    return;
                }
                server.allocation = match message.lifetime() {
                    Some(lifetime) if !lifetime.is_zero() => Allocation::granted(lifetime, now),
                    _ => {
                        tracing::warn!(
                            link = self.link,
                            server = server.name,
                            "TURN refresh granted no lifetime; its relayed candidate is dead"
                        );
                        Allocation::Ended
                    }
                };
            }
            Kind::Permit(ip) => {
                tracing::debug!(link = self.link, server = server.name, %ip, "TURN permission installed");
                server
                    .permissions
                    .insert(ip, Some(now + PERMISSION_REFRESH));
            }
            Kind::Release => {
                tracing::debug!(
                    link = self.link,
                    server = server.name,
                    "TURN allocation released"
                );
            }
        }
    }

    fn refused(&mut self, request: Request, message: &Message, now: Instant) {
        let code = message.error_code();
        let nonce = message.attribute(Attr::Nonce);
        let challenged = match code {
            Some(401) => request.nonce.is_none() || request.nonce.as_deref() != nonce,
            Some(438) => true,
            _ => false,
        };
        if challenged && request.attempt < ATTEMPTS && self.servers[request.server].learn(message) {
            self.ask(request.server, request.kind, request.attempt + 1, now);
        } else {
            self.failed(request, code);
        }
    }

    fn failed(&mut self, request: Request, code: Option<u16>) {
        let server = &mut self.servers[request.server];
        let reason = match code {
            Some(401) => "the credential was refused".to_string(),
            Some(code) => format!("error {code}"),
            None => "no answer".to_string(),
        };
        match request.kind {
            Kind::Allocate => {
                tracing::info!(
                    link = self.link,
                    server = server.name,
                    reason,
                    "TURN allocation failed; no relayed candidate"
                );
                server.allocation = Allocation::Ended;
            }
            Kind::Refresh => {
                if matches!(server.allocation, Allocation::Live { .. }) {
                    tracing::warn!(
                        link = self.link,
                        server = server.name,
                        reason,
                        "TURN refresh failed; its relayed candidate is dead"
                    );
                    server.allocation = Allocation::Ended;
                }
            }
            Kind::Permit(ip) => {
                tracing::warn!(link = self.link, server = server.name, %ip, reason, "TURN permission failed");
                server.permissions.remove(&ip);
            }
            Kind::Release => {
                tracing::debug!(
                    link = self.link,
                    server = server.name,
                    reason,
                    "TURN release failed"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stun::tests::error_code;

    const REALM: &str = "relay.example";

    fn server() -> SocketAddr {
        "192.0.2.10:3478".parse().unwrap()
    }

    fn relayed() -> SocketAddrV4 {
        "192.0.2.10:3478".parse().unwrap()
    }

    fn key() -> [u8; 16] {
        stun::long_term_key("1767225600:link", REALM, "secret")
    }

    fn relays(now: Instant) -> Relays {
        let spec = TurnServer {
            address: "turn.example.net:3478".into(),
            username: "1767225600:link".into(),
            credential: "secret".into(),
        };
        Relays::new("test", vec![(server(), &spec)], now)
    }

    fn sent(relays: &mut Relays) -> Vec<u8> {
        let (to, datagram) = relays.poll_transmit().expect("a datagram to the server");
        assert_eq!(to, server());
        assert!(relays.poll_transmit().is_none(), "only one datagram");
        datagram
    }

    fn reply(relays: &mut Relays, reply: Vec<u8>, now: Instant) {
        assert!(matches!(
            relays.receive(&reply, server(), now),
            Received::Consumed
        ));
    }

    fn refusal(request: &Message, code: u16, nonce: &str) -> Vec<u8> {
        MessageBuilder::new(request.method, Class::Error, request.transaction)
            .attribute(Attr::ErrorCode, &error_code(code))
            .attribute(Attr::Realm, REALM.as_bytes())
            .attribute(Attr::Nonce, nonce.as_bytes())
            .finish()
    }

    fn granted(request: &Message, lifetime: u32) -> Vec<u8> {
        let success = MessageBuilder::new(request.method, Class::Success, request.transaction)
            .attribute(Attr::Lifetime, &lifetime.to_be_bytes());
        match request.method {
            Method::Allocate => success.address(Attr::XorRelayedAddress, relayed()),
            _ => success,
        }
        .integrity(&key())
    }

    fn allocated(lifetime: u32, now: Instant) -> Relays {
        let mut relays = relays(now);
        let first = sent(&mut relays);
        let first = Message::parse(&first).unwrap();
        reply(&mut relays, refusal(&first, 401, "n1"), now);
        let signed = sent(&mut relays);
        let signed = Message::parse(&signed).unwrap();
        reply(&mut relays, granted(&signed, lifetime), now);
        relays
    }

    #[test]
    fn an_allocation_answers_the_challenge_and_refreshes_at_half_its_granted_lifetime() {
        let start = Instant::now();
        let mut relays = relays(start);
        assert!(!relays.settled());
        let first = sent(&mut relays);
        let first = Message::parse(&first).unwrap();
        assert_eq!(
            (first.method, first.class),
            (Method::Allocate, Class::Request)
        );
        assert_eq!(
            first.attribute(Attr::RequestedTransport),
            Some(&[17, 0, 0, 0][..])
        );
        assert_eq!(first.integrity(&key()), None);

        reply(&mut relays, refusal(&first, 401, "n1"), start);
        let signed = sent(&mut relays);
        let signed = Message::parse(&signed).unwrap();
        assert_eq!(signed.method, Method::Allocate);
        assert_eq!(
            signed.attribute(Attr::RequestedTransport),
            Some(&[17, 0, 0, 0][..])
        );
        assert_eq!(
            signed.attribute(Attr::Username),
            Some(&b"1767225600:link"[..])
        );
        assert_eq!(signed.attribute(Attr::Realm), Some(REALM.as_bytes()));
        assert_eq!(signed.attribute(Attr::Nonce), Some(&b"n1"[..]));
        assert_eq!(signed.integrity(&key()), Some(true));
        assert!(relays.relayed().next().is_none());

        reply(&mut relays, granted(&signed, 60), start);
        assert!(relays.settled());
        assert_eq!(
            relays.relayed().collect::<Vec<_>>(),
            [SocketAddr::V4(relayed())]
        );

        relays.handle_timeout(start + Duration::from_secs(29));
        assert!(relays.poll_transmit().is_none(), "refreshed too early");
        assert_eq!(relays.timeout(), Some(start + Duration::from_secs(30)));
        let later = start + Duration::from_secs(30);
        relays.handle_timeout(later);
        let refresh = sent(&mut relays);
        let refresh = Message::parse(&refresh).unwrap();
        assert_eq!(
            (refresh.method, refresh.integrity(&key())),
            (Method::Refresh, Some(true))
        );

        reply(&mut relays, refusal(&refresh, 438, "n2"), later);
        let retried = sent(&mut relays);
        let retried = Message::parse(&retried).unwrap();
        assert_eq!(retried.method, Method::Refresh);
        assert_eq!(retried.attribute(Attr::Nonce), Some(&b"n2"[..]));
        assert_eq!(retried.integrity(&key()), Some(true));
        reply(&mut relays, granted(&retried, 60), later);
        assert_eq!(relays.timeout(), Some(later + Duration::from_secs(30)));
        assert_eq!(relays.relayed().count(), 1);
    }

    #[test]
    fn a_refused_credential_ends_the_relayed_address() {
        let start = Instant::now();
        let mut relays = relays(start);
        let first = sent(&mut relays);
        reply(
            &mut relays,
            refusal(&Message::parse(&first).unwrap(), 401, "n1"),
            start,
        );
        let signed = sent(&mut relays);
        reply(
            &mut relays,
            refusal(&Message::parse(&signed).unwrap(), 401, "n1"),
            start,
        );
        assert!(relays.settled());
        assert!(relays.relayed().next().is_none());
        assert!(relays.poll_transmit().is_none(), "asked again");

        let mut relays = allocated(60, start);
        let later = start + Duration::from_secs(30);
        relays.handle_timeout(later);
        let refresh = sent(&mut relays);
        reply(
            &mut relays,
            refusal(&Message::parse(&refresh).unwrap(), 401, "n1"),
            later,
        );
        assert!(relays.relayed().next().is_none());
        let peer = "198.51.100.7:4000".parse().unwrap();
        assert!(
            relays
                .indication(SocketAddr::V4(relayed()), peer, b"rtp")
                .is_none()
        );
        assert!(relays.poll_transmit().is_none());
    }

    #[test]
    fn permissions_are_asked_once_per_address_and_renewed_before_five_minutes() {
        let start = Instant::now();
        let mut relays = allocated(600, start);
        let remote: IpAddr = "198.51.100.7".parse().unwrap();
        relays.permit(&[remote, remote, "2001:db8::1".parse().unwrap()], start);
        let asked = sent(&mut relays);
        let asked = Message::parse(&asked).unwrap();
        assert_eq!(asked.method, Method::CreatePermission);
        assert_eq!(
            asked
                .address(Attr::XorPeerAddress)
                .map(|peer| IpAddr::V4(*peer.ip())),
            Some(remote)
        );
        assert_eq!(asked.integrity(&key()), Some(true));
        reply(&mut relays, granted(&asked, 0), start);
        relays.permit(&[remote], start);
        assert!(relays.poll_transmit().is_none(), "asked twice");

        relays.handle_timeout(start + Duration::from_secs(239));
        assert!(relays.poll_transmit().is_none(), "renewed too early");
        relays.handle_timeout(start + Duration::from_secs(240));
        let renewed = sent(&mut relays);
        let renewed = Message::parse(&renewed).unwrap();
        assert_eq!(renewed.method, Method::CreatePermission);
        assert_eq!(
            renewed.address(Attr::XorPeerAddress),
            asked.address(Attr::XorPeerAddress)
        );
    }

    #[test]
    fn data_is_unwrapped_from_the_server_and_sent_wrapped_to_it() {
        let start = Instant::now();
        let mut relays = allocated(600, start);
        let peer: SocketAddrV4 = "198.51.100.7:4000".parse().unwrap();
        let data = MessageBuilder::new(Method::Data, Class::Indication, stun::transaction_id())
            .address(Attr::XorPeerAddress, peer)
            .attribute(Attr::Data, b"dtls")
            .finish();
        match relays.receive(&data, server(), start) {
            Received::Data {
                relayed: to,
                peer: from,
                payload,
            } => {
                assert_eq!(
                    (to, from, payload),
                    (
                        SocketAddr::V4(relayed()),
                        SocketAddr::V4(peer),
                        &b"dtls"[..]
                    )
                );
            }
            _ => panic!("the Data indication was not unwrapped"),
        }
        let elsewhere = "198.51.100.8:3478".parse().unwrap();
        assert!(matches!(
            relays.receive(&data, elsewhere, start),
            Received::Other
        ));
        let check = stun::binding_request(stun::transaction_id());
        assert!(matches!(
            relays.receive(&check, server(), start),
            Received::Other
        ));
        let rtp = [0x80, 111, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(matches!(
            relays.receive(&rtp, server(), start),
            Received::Other
        ));

        let (to, send) = relays
            .indication(SocketAddr::V4(relayed()), SocketAddr::V4(peer), b"rtp")
            .expect("a Send indication");
        assert_eq!(to, server());
        let send = Message::parse(&send).unwrap();
        assert_eq!((send.method, send.class), (Method::Send, Class::Indication));
        assert_eq!(send.address(Attr::XorPeerAddress), Some(peer));
        assert_eq!(send.attribute(Attr::Data), Some(&b"rtp"[..]));
        let host = "10.0.0.2:5000".parse().unwrap();
        assert!(
            relays
                .indication(host, SocketAddr::V4(peer), b"rtp")
                .is_none()
        );
    }

    #[test]
    fn an_unanswered_allocation_is_resent_and_then_given_up() {
        let start = Instant::now();
        let mut relays = relays(start);
        let first = sent(&mut relays);
        let mut at = start;
        let mut resent = 0;
        while !relays.settled() {
            at = relays.timeout().expect("a resend is due");
            relays.handle_timeout(at);
            if let Some((_, again)) = relays.poll_transmit() {
                assert_eq!(again, first);
                resent += 1;
            }
        }
        assert_eq!(resent, TRANSMISSIONS - 1);
        assert!(
            at - start < Duration::from_secs(20),
            "gave up after {:?}",
            at - start
        );
        assert!(relays.relayed().next().is_none());
    }
}
