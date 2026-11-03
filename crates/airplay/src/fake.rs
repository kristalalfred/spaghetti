use std::io::{Cursor, ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, Tag};
use plist::{Dictionary, Value};

use crate::browse::{Event, InstanceKey, Kind};
use crate::clock::{Clock, Ntp};
use crate::ptp::{Endpoint, Peer, Ptp, Timing};
use crate::receiver::tests::txt;
use crate::rtsp::{PLIST, Response, decode_response};
use crate::tlv8::{self, Tlv};
use crate::{Browser, FRAMES_PER_PACKET, ReceiverId, SAMPLE_RATE, hap, pairing, srp};

mod buffered;

pub(crate) use buffered::{BufferedPacket, DataReader, Reading};

const ID: &str = "AA:BB:CC:DD:EE:FF";
const INSTANCE: &str = "AABBCCDDEEFF@fake";
const POLL: Duration = Duration::from_millis(5);
/// Claude: what the fake's `_airplay._tcp` record says when it takes AirPlay 2
/// in realtime only: the HK Citation MultiBeam 1100's feature bits without 40
/// (buffered audio) and 41 (PTP).
pub(crate) const REALTIME_AIRPLAY_2_RECORD: &str =
    "deviceid=AA:BB:CC:DD:EE:FF features=0x445F8A00,0x1C040 flags=0x4 model=Fake";
/// Claude: what the fake's `_airplay._tcp` record says when it takes buffered
/// AirPlay 2 timed by PTP: the HK Citation MultiBeam 1100's feature bits.
pub(crate) const BUFFERED_AIRPLAY_2_RECORD: &str =
    "deviceid=AA:BB:CC:DD:EE:FF features=0x445F8A00,0x1C340 flags=0x4 model=Fake";
pub(crate) const STREAM_ID: u64 = 1;

/// Claude: a step of a handshake, at which the fake can hang or refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Announce,
    PairM1,
    PairM3,
    /// Claude: a session's SETUP with NTP timing.
    SessionSetup,
    /// Claude: a realtime stream's SETUP.
    StreamSetup,
    PtpSessionSetup,
    BufferedStreamSetup,
    Record,
    Flush,
    Feedback,
    SetPeers,
    Anchor,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Rtsp {
    #[default]
    Answer,
    /// Claude: answers nothing from this step on.
    HangAt(Step),
    /// Claude: answers nothing at this step only.
    IgnoreAt(Step),
    /// Claude: answers this step only after this long.
    SlowAt(Step, Duration),
    RefuseAt(Step, u16),
    /// Claude: answers this step with something that is not RTSP.
    MalformedAt(Step),
    /// Claude: closes the connection at this step instead of answering.
    CloseAt(Step),
    SilentAfterRecord,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Pairing {
    #[default]
    Honest,
    ErrorAtM2(u8),
    WrongProof,
}

/// Claude: what the session's SETUP says about the event port.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum EventPort {
    #[default]
    Listening,
    Closed,
    Missing,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Behavior {
    pub(crate) auth_setup_required: bool,
    pub(crate) audio_latency: Option<u32>,
    pub(crate) rtsp: Rtsp,
    pub(crate) pairing: Pairing,
    pub(crate) event_port: EventPort,
    /// Claude: how many anchors the fake answers 400 before taking one, as a
    /// receiver that has not yet measured the sender's clock does.
    pub(crate) anchor_refusals: u32,
    /// Claude: answers anchors 400 until its own timing with the sender has
    /// settled.
    pub(crate) anchor_until_timed: bool,
    pub(crate) reading: Reading,
}

#[derive(Debug, Clone)]
pub(crate) enum Seen {
    Request {
        at: Instant,
        connection: usize,
        request: FakeRequest,
    },
    Closed {
        at: Instant,
        connection: usize,
    },
    Audio {
        at: Instant,
        bytes: Vec<u8>,
    },
    Control {
        bytes: Vec<u8>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct FakeRequest {
    pub(crate) method: String,
    pub(crate) uri: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl FakeRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub(crate) fn plist(&self) -> Dictionary {
        Value::from_reader(Cursor::new(&self.body))
            .ok()
            .and_then(Value::into_dictionary)
            .expect("a binary plist dictionary")
    }

    pub(crate) fn tlv(&self) -> Tlv {
        Tlv::decode(&self.body).expect("TLV8 items")
    }
}

/// Claude: what the fake learns from a stream and shares between its threads.
#[derive(Default)]
struct State {
    seen: Mutex<Vec<Seen>>,
    control_port: Mutex<Option<u16>>,
    timing_port: Mutex<Option<u16>>,
    audio_key: Mutex<Option<[u8; 32]>>,
    events: Mutex<Events>,
    connections: Mutex<Vec<TcpStream>>,
    open_at_accept: Mutex<Vec<Vec<usize>>>,
    anchors: Mutex<Vec<(Instant, Dictionary)>>,
    data: Mutex<Data>,
    clock: Mutex<Option<(Arc<Ptp>, Endpoint)>>,
    timing: Mutex<Option<Peer>>,
    reading_released: AtomicBool,
    stop: AtomicBool,
}

/// Claude: what came over a buffered stream's data connection.
#[derive(Default)]
struct Data {
    key: Option<[u8; 32]>,
    received: Vec<(Instant, Vec<u8>)>,
    reader: DataReader,
    packets: Vec<(Instant, BufferedPacket)>,
    bytes: usize,
    ended: Option<Instant>,
}

/// Claude: the fake's side of a stream's event connection: it sends requests
/// sealed with its own counter and keeps the answers it opens.
#[derive(Default)]
struct Events {
    sending: Option<(TcpStream, hap::Channel)>,
    next_cseq: u32,
    answers: Vec<Response>,
}

/// Claude: a minimal AirPlay receiver on localhost that records everything a
/// stream sends it and answers as `Behavior` says. Like a real speaker, it
/// takes AirPlay 1 and AirPlay 2 on one port.
pub(crate) struct Fake {
    address: SocketAddr,
    listener: TcpListener,
    control: UdpSocket,
    timing: UdpSocket,
    state: Arc<State>,
}

impl Fake {
    pub(crate) fn start(behavior: Behavior) -> Fake {
        let fake = Fake::asleep(behavior);
        fake.wake();
        fake
    }

    /// Claude: a receiver whose RTSP port answers no connection until `wake`,
    /// as a speaker's does in standby.
    pub(crate) fn asleep(behavior: Behavior) -> Fake {
        let localhost = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let listener = bound_without_listening();
        listener.set_nonblocking(true).unwrap();
        let audio = UdpSocket::bind(localhost).unwrap();
        let control = UdpSocket::bind(localhost).unwrap();
        let timing = UdpSocket::bind(localhost).unwrap();
        for socket in [&audio, &control, &timing] {
            socket
                .set_read_timeout(Some(Duration::from_millis(20)))
                .unwrap();
        }
        let ports = Ports {
            audio: audio.local_addr().unwrap().port(),
            control: control.local_addr().unwrap().port(),
            timing: timing.local_addr().unwrap().port(),
        };
        let fake = Fake {
            address: listener.local_addr().unwrap(),
            listener: listener.try_clone().unwrap(),
            control: control.try_clone().unwrap(),
            timing,
            state: Arc::default(),
        };
        for (socket, audio) in [(audio, true), (control, false)] {
            let state = fake.state.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 2048];
                while !state.stop.load(Ordering::Acquire) {
                    if let Ok(len) = socket.recv(&mut buf) {
                        let (at, bytes) = (Instant::now(), buf[..len].to_vec());
                        let seen_now = if audio {
                            Seen::Audio { at, bytes }
                        } else {
                            Seen::Control { bytes }
                        };
                        state.seen.lock().unwrap().push(seen_now);
                    }
                }
            });
        }
        let state = fake.state.clone();
        thread::spawn(move || {
            let mut count = 0;
            while !state.stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let mut connections = state.connections.lock().unwrap();
                        let open = connections
                            .iter()
                            .enumerate()
                            .filter(|(_, earlier)| !at_end(earlier))
                            .map(|(connection, _)| connection)
                            .collect();
                        state.open_at_accept.lock().unwrap().push(open);
                        connections.push(stream.try_clone().unwrap());
                        drop(connections);
                        let state = state.clone();
                        let connection = count;
                        count += 1;
                        thread::spawn(move || {
                            Server {
                                stream,
                                connection,
                                behavior,
                                ports,
                                state,
                                channel: None,
                                secret: None,
                            }
                            .serve();
                        });
                    }
                    Err(_) => thread::sleep(POLL),
                }
            }
        });
        fake
    }

    pub(crate) fn wake(&self) {
        assert_eq!(unsafe { libc::listen(self.listener.as_raw_fd(), 16) }, 0);
    }

    pub(crate) fn address(&self) -> SocketAddr {
        self.address
    }

    pub(crate) fn id(&self) -> ReceiverId {
        ID.parse().unwrap()
    }

    /// Claude: a browser that has found this receiver with the given TXT record.
    pub(crate) fn browser(&self, record: &str) -> Browser {
        let browser = Browser::offline();
        self.advertise(&browser, record);
        browser
    }

    pub(crate) fn advertise(&self, browser: &Browser, record: &str) {
        self.advertise_on(browser, 1, record);
    }

    /// Claude: advertises the AirPlay 1 service as seen on `interface`, so
    /// that fakes on several interfaces are one receiver with an address each.
    pub(crate) fn advertise_on(&self, browser: &Browser, interface: u32, record: &str) {
        self.advertise_as(browser, Kind::Raop, INSTANCE, interface, record);
    }

    /// Claude: withdraws the AirPlay 1 service advertised on `interface`.
    pub(crate) fn withdraw(&self, browser: &Browser, interface: u32) {
        browser.shared().apply([Event::Removed(InstanceKey {
            kind: Kind::Raop,
            name: INSTANCE.into(),
            interface,
        })]);
    }

    /// Claude: advertises only the AirPlay 2 service, with `record`.
    pub(crate) fn advertise_airplay(&self, browser: &Browser, record: &str) {
        self.advertise_as(browser, Kind::AirPlay, "fake", 1, record);
    }

    fn advertise_as(
        &self,
        browser: &Browser,
        kind: Kind,
        name: &str,
        interface: u32,
        record: &str,
    ) {
        let key = InstanceKey {
            kind,
            name: name.into(),
            interface,
        };
        browser.shared().apply([
            Event::Resolved {
                key: key.clone(),
                host: "fake.local.".into(),
                port: self.address.port(),
                txt: txt(record),
            },
            Event::AddressAdded {
                key,
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                scope: 0,
            },
        ]);
    }

    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.state.seen.lock().unwrap().clone()
    }

    pub(crate) fn requests(&self) -> Vec<(Instant, usize, FakeRequest)> {
        self.seen()
            .into_iter()
            .filter_map(|seen| match seen {
                Seen::Request {
                    at,
                    connection,
                    request,
                } => Some((at, connection, request)),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn methods(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .map(|(_, _, r)| r.method)
            .collect()
    }

    /// Claude: each audio packet as it arrived, opened with the stream's key
    /// when it was sealed.
    pub(crate) fn audio(&self) -> Vec<(Instant, AudioPacket)> {
        self.seen()
            .into_iter()
            .filter_map(|seen| match seen {
                Seen::Audio { at, bytes } => Some((at, decode_audio(&self.open(&bytes)))),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn audio_bytes(&self) -> Vec<Vec<u8>> {
        self.seen()
            .into_iter()
            .filter_map(|seen| match seen {
                Seen::Audio { bytes, .. } => Some(bytes),
                _ => None,
            })
            .collect()
    }

    /// Claude: an audio packet's RTP header and plaintext payload. A sealed
    /// packet opens, as an AirPlay 2 receiver opens it, with the key the
    /// stream's SETUP sent, or the test fails.
    pub(crate) fn open(&self, bytes: &[u8]) -> Vec<u8> {
        let Some(key) = *self.state.audio_key.lock().unwrap() else {
            return bytes.to_vec();
        };
        let (sealed, trailer) = bytes.split_at(bytes.len() - 24);
        let (tag, counter) = trailer.split_at(16);
        let mut nonce = Nonce::default();
        nonce[4..].copy_from_slice(counter);
        let mut payload = sealed[12..].to_vec();
        ChaCha20Poly1305::new(&key.into())
            .decrypt_in_place_detached(&nonce, &bytes[4..12], &mut payload, Tag::from_slice(tag))
            .expect("the packet opens with the stream's key");
        [&bytes[..12], &payload[..]].concat()
    }

    pub(crate) fn control(&self) -> Vec<Vec<u8>> {
        self.seen()
            .into_iter()
            .filter_map(|seen| match seen {
                Seen::Control { bytes, .. } => Some(bytes),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn syncs(&self) -> Vec<SyncPacket> {
        self.control()
            .iter()
            .filter(|b| b[1] == 0xD4)
            .map(|b| decode_sync(b))
            .collect()
    }

    pub(crate) fn closed(&self, connection: usize) -> Option<Instant> {
        self.seen().into_iter().find_map(|seen| match seen {
            Seen::Closed { at, connection: c } if c == connection => Some(at),
            _ => None,
        })
    }

    fn client(&self, port: &Mutex<Option<u16>>) -> SocketAddr {
        let port = port.lock().unwrap().expect("the stream has said its port");
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    }

    /// Claude: sends a timing request saying it was sent at `sent`, and returns
    /// the reply with the instants around the exchange.
    pub(crate) fn time(&self, sent: Ntp) -> (Instant, [u8; 32], Instant) {
        let mut request = [0u8; 32];
        request[..4].copy_from_slice(&[0x80, 0xD2, 0x00, 0x07]);
        request[24..32].copy_from_slice(&sent.to_be_bytes());
        let before = Instant::now();
        self.timing
            .send_to(&request, self.client(&self.state.timing_port))
            .unwrap();
        let mut reply = [0u8; 32];
        let deadline = before + Duration::from_secs(1);
        loop {
            match self.timing.recv(&mut reply) {
                Ok(32) => return (before, reply, Instant::now()),
                _ if Instant::now() > deadline => panic!("no timing reply"),
                _ => {}
            }
        }
    }

    pub(crate) fn request_retransmission(&self, first: u16, count: u16) {
        let mut request = [0x80, 0xD5, 0x00, 0x01, 0, 0, 0, 0];
        request[4..6].copy_from_slice(&first.to_be_bytes());
        request[6..8].copy_from_slice(&count.to_be_bytes());
        self.control
            .send_to(&request, self.client(&self.state.control_port))
            .unwrap();
    }

    /// Claude: sends a request over the stream's event connection and returns
    /// its CSeq.
    pub(crate) fn send_event(&self) -> u32 {
        let mut events = self.state.events.lock().unwrap();
        events.next_cseq += 1;
        let cseq = events.next_cseq;
        let (stream, channel) = events
            .sending
            .as_mut()
            .expect("the stream opened its event connection");
        stream
            .write_all(&channel.seal(&event_request(cseq)))
            .unwrap();
        cseq
    }

    pub(crate) fn event_answers(&self) -> Vec<Response> {
        self.state.events.lock().unwrap().answers.clone()
    }

    /// Claude: the earlier connections the sender still held open when the
    /// fake accepted `connection`.
    pub(crate) fn open_when_accepted(&self, connection: usize) -> Vec<usize> {
        self.state.open_at_accept.lock().unwrap()[connection].clone()
    }

    /// Claude: how many TCP connections the fake has accepted.
    pub(crate) fn accepted(&self) -> usize {
        self.state.connections.lock().unwrap().len()
    }

    /// Claude: gives the fake a PTP clock, which times with the sender's
    /// clock at `sender` from the PTP session SETUP on, as a receiver starts
    /// timing once that SETUP names the sender.
    pub(crate) fn time_with(&self, clock: Arc<Ptp>, sender: Endpoint) {
        *self.state.clock.lock().unwrap() = Some((clock, sender));
    }

    /// Claude: ends the fake's timing with the sender, so that its clock
    /// goes silent to the sender.
    pub(crate) fn stop_timing(&self) {
        self.state.timing.lock().unwrap().take();
    }

    /// Claude: lets a data connection read `UntilReleased` read on.
    pub(crate) fn release_reading(&self) {
        self.state.reading_released.store(true, Ordering::Release);
    }

    /// Claude: where the fake's timing with the sender stands, once it has
    /// begun.
    pub(crate) fn timing(&self) -> Option<Timing> {
        self.state.timing.lock().unwrap().as_ref().map(Peer::timing)
    }

    /// Claude: each SETRATEANCHORTIME's body, as it came, whatever the fake
    /// answered.
    pub(crate) fn anchors(&self) -> Vec<(Instant, Dictionary)> {
        self.state.anchors.lock().unwrap().clone()
    }

    /// Claude: each packet of the buffered stream's data connection as it
    /// came, opened with the key the stream's SETUP sent.
    pub(crate) fn buffered_audio(&self) -> Vec<(Instant, BufferedPacket)> {
        let mut data = self.state.data.lock().unwrap();
        data.open_received();
        data.packets.clone()
    }

    /// Claude: how many bytes the fake has read from the data connection.
    pub(crate) fn data_bytes(&self) -> usize {
        self.state.data.lock().unwrap().bytes
    }

    /// Claude: when the data connection closed, as the fake read it.
    pub(crate) fn data_ended(&self) -> Option<Instant> {
        self.state.data.lock().unwrap().ended
    }

    pub(crate) fn close_connections(&self) {
        for connection in self.state.connections.lock().unwrap().iter() {
            let _ = connection.shutdown(Shutdown::Both);
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        self.close_connections();
        if let Some((stream, _)) = &self.state.events.lock().unwrap().sending {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

/// Claude: whether the other end has closed `stream`, which a read would
/// show, looking without taking anything or waiting.
fn at_end(stream: &TcpStream) -> bool {
    let mut byte = 0u8;
    let peeked = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&raw mut byte).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    peeked == 0
}

/// Claude: a TCP socket bound to a localhost port but not listening on it.
/// macOS answers no connection to the port, and no other socket can take it.
fn bound_without_listening() -> TcpListener {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    assert!(fd >= 0);
    let socket = unsafe { TcpListener::from_raw_fd(fd) };
    let address = libc::sockaddr_in {
        sin_len: size_of::<libc::sockaddr_in>() as u8,
        sin_family: libc::AF_INET as u8,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from(Ipv4Addr::LOCALHOST).to_be(),
        },
        sin_zero: [0; 8],
    };
    let len = size_of_val(&address) as libc::socklen_t;
    assert_eq!(
        unsafe { libc::bind(fd, (&raw const address).cast(), len) },
        0
    );
    socket
}

#[derive(Debug, Clone, Copy)]
struct Ports {
    audio: u16,
    control: u16,
    timing: u16,
}

/// Claude: one RTSP connection to the fake.
struct Server {
    stream: TcpStream,
    connection: usize,
    behavior: Behavior,
    ports: Ports,
    state: Arc<State>,
    channel: Option<hap::Channel>,
    secret: Option<[u8; srp::PROOF_LEN]>,
}

/// Claude: the fake's SRP verifier and public value, computed once: a real
/// receiver draws a salt and a private value for each pairing, but the fake
/// spares a debug build that arithmetic.
fn transient_server() -> &'static srp::Server {
    static SERVER: std::sync::OnceLock<srp::Server> = std::sync::OnceLock::new();
    SERVER.get_or_init(|| {
        let mut salt = [0u8; 16];
        let mut private = [0u8; 32];
        getrandom::fill(&mut salt).unwrap();
        getrandom::fill(&mut private).unwrap();
        srp::Server::new(pairing::USER, pairing::TRANSIENT_PIN, &salt, &private)
    })
}

/// Claude: what the fake answers a request with, before any encryption.
struct Answer {
    status: &'static str,
    headers: String,
    body: Vec<u8>,
    content_type: Option<&'static str>,
}

impl Answer {
    fn ok() -> Answer {
        Answer::status("200 OK")
    }

    fn status(status: &'static str) -> Answer {
        Answer {
            status,
            headers: String::new(),
            body: Vec::new(),
            content_type: None,
        }
    }

    fn with_body(mut self, content_type: &'static str, body: Vec<u8>) -> Answer {
        self.content_type = Some(content_type);
        self.body = body;
        self
    }
}

impl Server {
    fn serve(mut self) {
        self.stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let mut received = Vec::new();
        let mut auth_setup = false;
        let mut silent = false;
        let mut recorded = false;
        loop {
            let request = loop {
                if let Some((request, len)) = parse_request(&received) {
                    received.drain(..len);
                    break Some(request);
                }
                let mut chunk = [0u8; 4096];
                match self.stream.read(&mut chunk) {
                    Ok(0) => break None,
                    Ok(n) => match &mut self.channel {
                        Some(channel) => channel
                            .open(&chunk[..n], &mut received)
                            .expect("requests open with the control keys"),
                        None => received.extend_from_slice(&chunk[..n]),
                    },
                    Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                        if self.state.stop.load(Ordering::Acquire) {
                            break None;
                        }
                    }
                    Err(_) => break None,
                }
            };
            let Some(request) = request else {
                self.state.seen.lock().unwrap().push(Seen::Closed {
                    at: Instant::now(),
                    connection: self.connection,
                });
                return;
            };
            self.state.seen.lock().unwrap().push(Seen::Request {
                at: Instant::now(),
                connection: self.connection,
                request: request.clone(),
            });
            if silent {
                continue;
            }
            let step = step_of(&request);
            match (self.behavior.rtsp, step) {
                (Rtsp::HangAt(hang), Some(step)) if hang == step => {
                    silent = true;
                    continue;
                }
                (Rtsp::IgnoreAt(ignored), Some(step)) if ignored == step => continue,
                (Rtsp::CloseAt(closing), Some(step)) if closing == step => {
                    let _ = self.stream.shutdown(Shutdown::Both);
                    continue;
                }
                (Rtsp::MalformedAt(malformed), Some(step)) if malformed == step => {
                    let mut garbage = b"HTTP/1.1 nonsense\r\n\r\n".to_vec();
                    if let Some(channel) = &mut self.channel {
                        garbage = channel.seal(&garbage);
                    }
                    let _ = self.stream.write_all(&garbage);
                    continue;
                }
                (Rtsp::SlowAt(slow, delay), Some(step)) if slow == step => thread::sleep(delay),
                _ => {}
            }
            let answer = match (self.behavior.rtsp, step) {
                (Rtsp::RefuseAt(refused, status), Some(step)) if refused == step => {
                    Answer::status(status_line(status))
                }
                _ => match request.method.as_str() {
                    "OPTIONS" => {
                        let mut answer = Answer::ok();
                        answer.headers.push_str(
                            "Public: ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, POST\r\n",
                        );
                        answer
                    }
                    "POST" if request.uri == "/auth-setup" => {
                        auth_setup = true;
                        Answer::ok().with_body("application/octet-stream", vec![0x42; 32])
                    }
                    "POST" if request.uri == "/pair-setup" => self.pair(&request),
                    "GET" if request.uri == "/info" => {
                        let mut info = Dictionary::new();
                        info.insert("features".into(), 0x0001_C340_445F_8A00u64.into());
                        info.insert("pk".into(), Value::Data(vec![0x27; 32]));
                        let mut body = Vec::new();
                        Value::Dictionary(info).to_writer_binary(&mut body).unwrap();
                        Answer::ok().with_body(PLIST, body)
                    }
                    "ANNOUNCE" if self.behavior.auth_setup_required && !auth_setup => {
                        Answer::status("403 Forbidden")
                    }
                    "SETUP" if request.body.is_empty() => self.setup_raop(&request),
                    "SETUP" => self.setup_airplay_2(&request, recorded),
                    "SETRATEANCHORTIME" => self.anchor(&request),
                    "RECORD" => {
                        recorded = true;
                        let mut answer = Answer::ok();
                        if let Some(latency) = self.behavior.audio_latency {
                            answer
                                .headers
                                .push_str(&format!("Audio-Latency: {latency}\r\n"));
                        }
                        silent = self.behavior.rtsp == Rtsp::SilentAfterRecord;
                        answer
                    }
                    _ => Answer::ok(),
                },
            };
            let cseq = request.header("CSeq").unwrap_or("0");
            let content_type = answer
                .content_type
                .map(|t| format!("Content-Type: {t}\r\n"))
                .unwrap_or_default();
            let mut response = format!(
                "RTSP/1.0 {}\r\nCSeq: {cseq}\r\nServer: fake\r\n{}{content_type}Content-Length: {}\r\n\r\n",
                answer.status,
                answer.headers,
                answer.body.len()
            )
            .into_bytes();
            response.extend_from_slice(&answer.body);
            if let Some(channel) = &mut self.channel {
                response = channel.seal(&response);
            }
            if self.stream.write_all(&response).is_err() {
                return;
            }
            if self.channel.is_none()
                && let Some(secret) = self.secret
            {
                self.channel = Some(hap::Channel::new(&hap::keys(
                    &secret,
                    "Control-Salt",
                    "Control-Read-Encryption-Key",
                    "Control-Write-Encryption-Key",
                )));
            }
        }
    }

    /// Claude: the server side of transient pairing, as an AirPlay 2 receiver
    /// answers M1 and M3.
    fn pair(&mut self, request: &FakeRequest) -> Answer {
        let tlv = request.tlv();
        let answer = |items: &[(u8, &[u8])]| {
            Answer::ok().with_body("application/octet-stream", Tlv::encode(items))
        };
        match tlv.get(tlv8::STATE) {
            Some([1]) => {
                assert_eq!(tlv.get(tlv8::METHOD), Some(&[0][..]));
                assert_eq!(tlv.get(tlv8::FLAGS), Some(&[0x10][..]));
                if let Pairing::ErrorAtM2(error) = self.behavior.pairing {
                    return answer(&[(tlv8::STATE, &[2]), (tlv8::ERROR, &[error])]);
                }
                let server = transient_server();
                answer(&[
                    (tlv8::STATE, &[2]),
                    (tlv8::SALT, &server.salt),
                    (tlv8::PUBLIC_KEY, &server.public),
                ])
            }
            Some([3]) => {
                let server = transient_server();
                let client_public = tlv.get(tlv8::PUBLIC_KEY).expect("M3 has A");
                let proof = tlv.get(tlv8::PROOF).expect("M3 has a proof");
                let Some((mut server_proof, key)) = server.verify(client_public, proof) else {
                    return answer(&[(tlv8::STATE, &[4]), (tlv8::ERROR, &[2])]);
                };
                if self.behavior.pairing == Pairing::WrongProof {
                    server_proof[0] ^= 1;
                } else {
                    self.secret = Some(key);
                }
                answer(&[(tlv8::STATE, &[4]), (tlv8::PROOF, &server_proof)])
            }
            state => panic!("pair-setup in state {state:?}"),
        }
    }

    fn setup_raop(&self, request: &FakeRequest) -> Answer {
        let transport = request.header("Transport").unwrap_or_default();
        let port = |key: &str| {
            transport
                .split(';')
                .find_map(|part| part.strip_prefix(key)?.strip_prefix('='))
                .and_then(|p| p.parse().ok())
                .unwrap()
        };
        *self.state.control_port.lock().unwrap() = Some(port("control_port"));
        *self.state.timing_port.lock().unwrap() = Some(port("timing_port"));
        let Ports {
            audio,
            control,
            timing,
        } = self.ports;
        let mut answer = Answer::ok();
        answer.headers = format!(
            "Session: DEADBEEF;timeout=60\r\nTransport: RTP/AVP/UDP;unicast;mode=record;\
             server_port={audio};control_port={control};timing_port={timing}\r\n"
        );
        answer
    }

    /// Claude: answers a session's SETUP with NTP or PTP timing, as
    /// shairport-sync answers, and a stream's, realtime or buffered. A
    /// buffered stream's SETUP before RECORD is refused, as receivers that
    /// play buffered audio only once recording are reported to need.
    fn setup_airplay_2(&self, request: &FakeRequest, recorded: bool) -> Answer {
        let setup = request.plist();
        let number = |dictionary: &Dictionary, key: &str| {
            dictionary[key]
                .as_unsigned_integer()
                .unwrap_or_else(|| panic!("{key} is a number"))
        };
        let reply = if let Some(streams) = setup.get("streams").and_then(Value::as_array) {
            let stream = streams[0].as_dictionary().unwrap();
            let key = stream["shk"].as_data().unwrap().try_into().unwrap();
            *self.state.audio_key.lock().unwrap() = Some(key);
            let kind = number(stream, "type");
            let mut reply = Dictionary::new();
            reply.insert("type".into(), kind.into());
            if kind == 103 {
                if !recorded {
                    return Answer::status(status_line(455));
                }
                let port =
                    buffered::listen_for_data(self.state.clone(), key, self.behavior.reading);
                reply.insert("dataPort".into(), u64::from(port).into());
                reply.insert("audioBufferSize".into(), 8_388_608u64.into());
            } else {
                *self.state.control_port.lock().unwrap() =
                    Some(number(stream, "controlPort") as u16);
                reply.insert("dataPort".into(), u64::from(self.ports.audio).into());
            }
            reply.insert("controlPort".into(), u64::from(self.ports.control).into());
            reply.insert("streamID".into(), STREAM_ID.into());
            let mut streams = Dictionary::new();
            streams.insert("streams".into(), vec![Value::Dictionary(reply)].into());
            streams
        } else {
            let secret = self.secret.expect("the stream paired first");
            let mut reply = Dictionary::new();
            if self.behavior.event_port != EventPort::Missing {
                let port = self.listen_for_events(secret);
                reply.insert("eventPort".into(), u64::from(port).into());
            }
            if setup["timingProtocol"].as_string() == Some("PTP") {
                assert!(setup.contains_key("timingPeerInfo"));
                if let Some((clock, sender)) = &*self.state.clock.lock().unwrap() {
                    let local = self.stream.local_addr().unwrap();
                    *self.state.timing.lock().unwrap() = Some(clock.add_peer(local, *sender));
                }
                let mut peer = Dictionary::new();
                peer.insert("Addresses".into(), vec![Value::from("127.0.0.1")].into());
                peer.insert("ID".into(), "127.0.0.1".into());
                reply.insert("timingPort".into(), 0u64.into());
                reply.insert("timingPeerInfo".into(), Value::Dictionary(peer));
            } else {
                *self.state.timing_port.lock().unwrap() = Some(number(&setup, "timingPort") as u16);
                reply.insert("timingPort".into(), u64::from(self.ports.timing).into());
            }
            reply
        };
        let mut body = Vec::new();
        Value::Dictionary(reply)
            .to_writer_binary(&mut body)
            .unwrap();
        Answer::ok().with_body(PLIST, body)
    }

    /// Claude: keeps each anchor and takes it once it has turned away
    /// `anchor_refusals` with 400.
    fn anchor(&self, request: &FakeRequest) -> Answer {
        let timed = matches!(
            self.state.timing.lock().unwrap().as_ref().map(Peer::timing),
            Some(Timing::Settled(_))
        );
        let mut anchors = self.state.anchors.lock().unwrap();
        anchors.push((Instant::now(), request.plist()));
        if anchors.len() as u32 <= self.behavior.anchor_refusals
            || (self.behavior.anchor_until_timed && !timed)
        {
            Answer::status(status_line(400))
        } else {
            Answer::ok()
        }
    }

    /// Claude: opens the port the stream's event connection comes to. Once it
    /// connects, the fake sends two requests in one write and keeps every
    /// answer.
    fn listen_for_events(&self, secret: [u8; srp::PROOF_LEN]) -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        if self.behavior.event_port == EventPort::Closed {
            return port;
        }
        listener.set_nonblocking(true).unwrap();
        let state = self.state.clone();
        let keys = hap::keys(
            &secret,
            "Events-Salt",
            "Events-Write-Encryption-Key",
            "Events-Read-Encryption-Key",
        );
        thread::spawn(move || {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(_) if state.stop.load(Ordering::Acquire) => return,
                    Err(_) => thread::sleep(POLL),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(20)))
                .unwrap();
            {
                let mut events = state.events.lock().unwrap();
                let mut sending = hap::Channel::new(&keys);
                let first = sending.seal(&event_request(1));
                let second = sending.seal(&event_request(2));
                stream.write_all(&[first, second].concat()).unwrap();
                events.next_cseq = 2;
                events.sending = Some((stream.try_clone().unwrap(), sending));
            }
            let mut opening = hap::Channel::new(&keys);
            let mut plaintext = Vec::new();
            let mut chunk = [0u8; 4096];
            while !state.stop.load(Ordering::Acquire) {
                match stream.read(&mut chunk) {
                    Ok(0) => return,
                    Ok(n) => opening
                        .open(&chunk[..n], &mut plaintext)
                        .expect("event answers open with the event keys"),
                    Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                        continue;
                    }
                    Err(_) => return,
                }
                while let Some((answer, len)) = decode_response(&plaintext).unwrap() {
                    plaintext.drain(..len);
                    state.events.lock().unwrap().answers.push(answer);
                }
            }
        });
        port
    }
}

fn event_request(cseq: u32) -> Vec<u8> {
    let mut body = Vec::new();
    let mut command = Dictionary::new();
    command.insert("type".into(), "updateMRSupportedCommands".into());
    Value::Dictionary(command)
        .to_writer_binary(&mut body)
        .unwrap();
    let mut request = format!(
        "POST /command RTSP/1.0\r\nCSeq: {cseq}\r\nServer: AirTunes/366.0\r\nContent-Type: {PLIST}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);
    request
}

fn step_of(request: &FakeRequest) -> Option<Step> {
    match (request.method.as_str(), request.uri.as_str()) {
        ("ANNOUNCE", _) => Some(Step::Announce),
        ("POST", "/pair-setup") => match request.tlv().get(tlv8::STATE) {
            Some([1]) => Some(Step::PairM1),
            _ => Some(Step::PairM3),
        },
        ("SETUP", _) if request.body.is_empty() => None,
        ("SETUP", _) => {
            let setup = request.plist();
            let buffered = setup
                .get("streams")
                .and_then(Value::as_array)
                .map(|streams| streams[0].as_dictionary().unwrap()["type"].as_unsigned_integer());
            match buffered {
                Some(Some(103)) => Some(Step::BufferedStreamSetup),
                Some(_) => Some(Step::StreamSetup),
                None if setup["timingProtocol"].as_string() == Some("PTP") => {
                    Some(Step::PtpSessionSetup)
                }
                None => Some(Step::SessionSetup),
            }
        }
        ("RECORD", _) => Some(Step::Record),
        ("FLUSH", _) => Some(Step::Flush),
        ("POST", "/feedback") => Some(Step::Feedback),
        ("SETPEERS", _) => Some(Step::SetPeers),
        ("SETRATEANCHORTIME", _) => Some(Step::Anchor),
        _ => None,
    }
}

fn status_line(status: u16) -> &'static str {
    match status {
        400 => "400 Bad Request",
        401 => "401 Unauthorized",
        403 => "403 Forbidden",
        453 => "453 Not Enough Bandwidth",
        455 => "455 Method Not Valid in This State",
        470 => "470 Connection Authorization Required",
        500 => "500 Internal Server Error",
        501 => "501 Not Implemented",
        _ => panic!("no status line for {status}"),
    }
}

fn parse_request(buf: &[u8]) -> Option<(FakeRequest, usize)> {
    let head_len = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buf[..head_len]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, uri) = (first.next()?.to_owned(), first.next()?.to_owned());
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let body_len = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("Content-Length"))
        .map_or(0, |(_, v)| v.parse().unwrap());
    let end = head_len + 4 + body_len;
    (buf.len() >= end).then(|| {
        (
            FakeRequest {
                method,
                uri,
                headers,
                body: buf[head_len + 4..end].to_vec(),
            },
            end,
        )
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AudioPacket {
    pub(crate) seq: u16,
    pub(crate) timestamp: u32,
    pub(crate) marker: bool,
    /// Claude: whether the ALAC frame carries its frame count.
    pub(crate) counted: bool,
    pub(crate) frames: Vec<[i16; 2]>,
}

/// Claude: reads an RTP packet carrying an uncompressed ALAC frame, checking
/// every field of the frame's layout. A frame without its count holds the
/// stream's frames per packet.
pub(crate) fn decode_audio(bytes: &[u8]) -> AudioPacket {
    assert_eq!(bytes[0], 0x80);
    assert_eq!(bytes[1] & 0x7F, 0x60);
    let (counted, frames) = decode_alac(&bytes[12..]);
    AudioPacket {
        seq: u16::from_be_bytes([bytes[2], bytes[3]]),
        timestamp: u32::from_be_bytes(bytes[4..8].try_into().unwrap()),
        marker: bytes[1] & 0x80 != 0,
        counted,
        frames,
    }
}

/// Claude: reads an uncompressed ALAC frame, checking every field of its
/// layout, and says whether it carries its count.
fn decode_alac(payload: &[u8]) -> (bool, Vec<[i16; 2]>) {
    let mut bits = BitReader {
        bytes: payload,
        bit: 0,
    };
    assert_eq!(bits.take(3), 1, "channel pair element");
    assert_eq!(bits.take(4), 0, "element instance");
    assert_eq!(bits.take(12), 0, "unused");
    let counted = bits.take(1) == 1;
    assert_eq!(bits.take(2), 0, "shift");
    assert_eq!(bits.take(1), 1, "not compressed");
    let count = if counted {
        bits.take(32) as usize
    } else {
        FRAMES_PER_PACKET
    };
    let frames = (0..count)
        .map(|_| [bits.take(16) as u16 as i16, bits.take(16) as u16 as i16])
        .collect();
    assert_eq!(bits.take(3), 7, "end element");
    assert_eq!(bits.bit.div_ceil(8), payload.len(), "no trailing bytes");
    (counted, frames)
}

struct BitReader<'a> {
    bytes: &'a [u8],
    bit: usize,
}

impl BitReader<'_> {
    fn take(&mut self, bits: u32) -> u32 {
        (0..bits).fold(0, |acc, _| {
            let bit = (self.bytes[self.bit / 8] >> (7 - self.bit % 8)) & 1;
            self.bit += 1;
            acc << 1 | u32::from(bit)
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SyncPacket {
    pub(crate) first: bool,
    pub(crate) seq: u16,
    pub(crate) playing: u32,
    pub(crate) at: Ntp,
    pub(crate) sending: u32,
}

fn decode_sync(bytes: &[u8]) -> SyncPacket {
    assert_eq!(bytes.len(), 20);
    SyncPacket {
        first: bytes[0] & 0x10 != 0,
        seq: u16::from_be_bytes([bytes[2], bytes[3]]),
        playing: u32::from_be_bytes(bytes[4..8].try_into().unwrap()),
        at: Ntp::from_be_bytes(bytes[8..16].try_into().unwrap()),
        sending: u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
    }
}

/// Claude: when a receiver plays the frame with RTP time `timestamp`, by
/// shairport-sync's reading of a sync packet: the frame `playing` plays at
/// `at`, delayed by the receiver's own Audio-Latency when the packet's
/// sequence field is 7.
pub(crate) fn play_time(
    sync: &SyncPacket,
    audio_latency: u32,
    timestamp: u32,
    clock: &Clock,
) -> Instant {
    let added = if sync.seq == 7 { audio_latency } else { 0 };
    let latency = sync.sending.wrapping_sub(sync.playing).wrapping_add(added);
    let anchor = sync.sending.wrapping_sub(latency);
    let offset = i64::from(timestamp.wrapping_sub(anchor) as i32);
    let nanos = Duration::from_nanos(
        (u128::from(offset.unsigned_abs()) * 1_000_000_000 / u128::from(SAMPLE_RATE)) as u64,
    );
    let at = clock.instant(sync.at);
    if offset >= 0 { at + nanos } else { at - nanos }
}

pub(crate) fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        thread::sleep(POLL);
    }
    done()
}

#[cfg(test)]
mod airplay2;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::clock::PACKET_DURATION;
    use crate::receiver::tests::{MOVE_2_RAOP, SHAIRPORT_SYNC_RAOP};
    use crate::rtsp::{Connection, OpenError};
    use crate::stream::Settings;
    use crate::{Protocol, Render, Status, Stream};
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    pub(crate) const LATENCY: Duration = Duration::from_millis(300);
    /// Claude: how soon a cancelled stream lets go, well short of the play-out,
    /// lookup and answer timeouts it would otherwise wait for.
    pub(crate) const PROMPT: Duration = Duration::from_millis(250);

    pub(crate) fn settings() -> Settings {
        Settings {
            latency: LATENCY,
            lookup_window: Duration::from_millis(500),
            connect_timeout: Duration::from_secs(1),
            wake_window: Duration::from_secs(1),
            answer_timeout: Duration::from_secs(1),
            teardown_timeout: Duration::from_millis(300),
            ..Settings::new().unwrap()
        }
    }

    pub(crate) fn expected_frame(n: usize, i: usize) -> [i16; 2] {
        if i == 0 {
            return [n as i16, -1];
        }
        let value = (n * 1009 + i * 31) % 65_536;
        [value as u16 as i16, !(value as u16) as i16]
    }

    /// Claude: what a stream asked its renderer for, call by call. Each packet
    /// carries its call number in its first frame.
    #[derive(Clone, Default)]
    pub(crate) struct Renders(Arc<Mutex<Vec<Instant>>>);

    impl Renders {
        pub(crate) fn renderer(
            &self,
            stall: Option<(usize, Duration)>,
        ) -> impl FnMut(Render<'_>) + Send + 'static {
            let calls = self.0.clone();
            move |render: Render<'_>| {
                let n = {
                    let mut calls = calls.lock().unwrap();
                    calls.push(render.play_at);
                    calls.len() - 1
                };
                let [left, right] = render.channels;
                for i in 0..FRAMES_PER_PACKET {
                    let [l, r] = expected_frame(n, i);
                    left[i] = f32::from(l) / 32768.0;
                    right[i] = f32::from(r) / 32768.0;
                }
                if let Some((call, duration)) = stall
                    && call == n
                {
                    thread::sleep(duration);
                }
            }
        }

        pub(crate) fn play_at(&self, call: usize) -> Instant {
            self.0.lock().unwrap()[call]
        }

        pub(crate) fn count(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    pub(crate) fn call_of(packet: &AudioPacket) -> usize {
        let call = packet.frames[0][0] as u16 as usize;
        for (i, frame) in packet.frames.iter().enumerate() {
            assert_eq!(*frame, expected_frame(call, i), "frame {i} of call {call}");
        }
        call
    }

    pub(crate) fn streaming(stream: &Stream) -> bool {
        matches!(stream.status(), Status::Streaming(_))
    }

    #[test]
    fn the_handshake_runs_in_order_and_record_names_the_first_packet() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let renders = Renders::default();
        let stream =
            Stream::start_with(&browser, fake.id(), Ok(settings()), renders.renderer(None));
        assert_eq!(stream.status(), Status::Connecting);
        assert!(wait_until(Duration::from_secs(2), || fake.audio().len() >= 10));
        stream.finish(Duration::ZERO).wait();

        assert_eq!(
            fake.methods(),
            ["OPTIONS", "ANNOUNCE", "SETUP", "RECORD", "TEARDOWN"]
        );
        let requests = fake.requests();
        let announce = &requests[1].2;
        assert_eq!(announce.header("Content-Type"), Some("application/sdp"));
        let sdp = String::from_utf8(announce.body.clone()).unwrap();
        assert!(sdp.contains("m=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\n"));
        assert!(sdp.contains("a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n"));
        let transport = requests[2].2.header("Transport").unwrap();
        assert!(
            transport.starts_with("RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=")
        );
        assert!(transport.contains(";timing_port="));
        assert!(requests.iter().all(
            |(_, _, r)| r.header("Client-Instance").is_some() && r.header("DACP-ID").is_some()
        ));
        for (_, _, request) in &requests[3..] {
            assert_eq!(request.header("Session"), Some("DEADBEEF"));
            assert_eq!(request.uri, announce.uri);
        }

        let audio = fake.audio();
        let first = &audio[0].1;
        let rtp_info = requests[3].2.header("RTP-Info").unwrap().to_owned();
        assert_eq!(
            rtp_info,
            format!("seq={};rtptime={}", first.seq, first.timestamp)
        );
        assert!(first.marker);
        assert!(audio[1..].iter().all(|(_, p)| !p.marker));
        for (i, (_, packet)) in audio.iter().enumerate() {
            assert_eq!(call_of(packet), i);
        }
    }

    #[test]
    fn timing_requests_are_answered_with_the_streams_clock() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || streaming(&stream)));
        let clock = stream.clock();
        for sent in [Ntp(0x0102_0304_0506_0708), Ntp(u64::MAX)] {
            let (before, reply, after) = fake.time(sent);
            assert_eq!(reply[..4], [0x80, 0xD3, 0x00, 0x07]);
            assert_eq!(Ntp::from_be_bytes(reply[8..16].try_into().unwrap()), sent);
            let received = clock.instant(Ntp::from_be_bytes(reply[16..24].try_into().unwrap()));
            let transmitted = clock.instant(Ntp::from_be_bytes(reply[24..32].try_into().unwrap()));
            let slack = Duration::from_micros(1);
            assert!(
                before <= received + slack
                    && received <= transmitted + slack
                    && transmitted <= after + slack
            );
        }
    }

    pub(crate) fn check_sync_mapping(
        protocol: Protocol,
        audio_latency: Option<u32>,
        first_seq: u16,
        first_rtp: u32,
    ) {
        let fake = Fake::start(Behavior {
            audio_latency,
            ..Behavior::default()
        });
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        fake.advertise_airplay(&browser, REALTIME_AIRPLAY_2_RECORD);
        let renders = Renders::default();
        let settings = Settings {
            first_seq: u32::from(first_seq),
            first_rtp,
            protocol: Some(protocol),
            ..settings()
        };
        let stream = Stream::start_with(&browser, fake.id(), Ok(settings), renders.renderer(None));
        assert!(wait_until(Duration::from_secs(10), || fake.syncs().len() >= 3));
        let clock = stream.clock();
        drop(stream);

        let syncs = fake.syncs();
        assert!(syncs[0].first);
        assert!(syncs[1..].iter().all(|s| !s.first));
        let audio = fake.audio();
        assert!(audio.len() > 200);
        for pair in audio.windows(2) {
            let (a, b) = (&pair[0].1, &pair[1].1);
            assert_eq!(b.seq, a.seq.wrapping_add(1));
            assert_eq!(
                b.timestamp,
                a.timestamp.wrapping_add(FRAMES_PER_PACKET as u32)
            );
        }
        let receiver_latency = audio_latency.unwrap_or(11_025);
        for sync in &syncs {
            for (_, packet) in &audio {
                let expected = renders.play_at(call_of(packet));
                let mapped = play_time(sync, receiver_latency, packet.timestamp, &clock);
                let error = if mapped > expected {
                    mapped - expected
                } else {
                    expected - mapped
                };
                assert!(
                    error < Duration::from_micros(5),
                    "packet {} plays {error:?} off",
                    packet.seq
                );
            }
        }
    }

    #[test]
    fn sync_packets_account_for_the_audio_latency_a_receiver_reports() {
        check_sync_mapping(Protocol::AirPlay1, Some(4_410), 1000, 5_000_000);
    }

    #[test]
    fn a_receiver_reporting_no_audio_latency_is_taken_to_add_a_quarter_second() {
        check_sync_mapping(Protocol::AirPlay1, None, 1000, 5_000_000);
    }

    #[test]
    fn sequence_numbers_and_rtp_times_wrap_without_a_seam() {
        check_sync_mapping(
            Protocol::AirPlay1,
            Some(11_025),
            65_530,
            u32::MAX - 3 * FRAMES_PER_PACKET as u32 + 100,
        );
    }

    #[test]
    fn retransmissions_resend_the_original_bytes_across_the_wrap() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let settings = Settings {
            first_seq: 65_533,
            ..settings()
        };
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || fake.audio().len() >= 20));
        fake.request_retransmission(65_534, 4);
        fake.request_retransmission(40_000, 1);
        assert!(wait_until(Duration::from_secs(1), || fake
            .control()
            .iter()
            .filter(|b| b[1] == 0xD6)
            .count()
            >= 4));
        thread::sleep(Duration::from_millis(50));
        let originals = fake.audio_bytes();
        let retransmissions: Vec<Vec<u8>> = fake
            .control()
            .into_iter()
            .filter(|b| b[1] == 0xD6)
            .collect();
        assert_eq!(retransmissions.len(), 4);
        for (retransmission, original) in retransmissions.iter().zip(&originals[1..5]) {
            assert_eq!(retransmission[2..4], original[2..4]);
            assert_eq!(&retransmission[4..], original.as_slice());
        }
        let Status::Streaming(counters) = stream.status() else {
            panic!("not streaming")
        };
        assert_eq!(counters.packets_retransmitted, 4);
    }

    #[test]
    fn a_slow_renderer_never_makes_a_packet_late() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let renders = Renders::default();
        let stall = Some((20, LATENCY + Duration::from_millis(200)));
        let stream =
            Stream::start_with(&browser, fake.id(), Ok(settings()), renders.renderer(stall));
        assert!(wait_until(Duration::from_secs(3), || renders.count() > 80));
        drop(stream);
        thread::sleep(Duration::from_millis(50));

        let audio = fake.audio();
        let calls: Vec<usize> = audio.iter().map(|(_, p)| call_of(p)).collect();
        assert!(
            !calls.contains(&20),
            "the stalled packet was sent after its play time"
        );
        assert!(
            calls.iter().any(|&c| c < 20),
            "packets before the stall were sent"
        );
        assert!(
            calls.iter().filter(|&&c| c > 20).count() > 30,
            "the stream resumed after the stall"
        );
        for (arrived, packet) in &audio {
            assert!(
                *arrived < renders.play_at(call_of(packet)),
                "packet {} arrived late",
                packet.seq
            );
        }

        let first = renders.play_at(0);
        let play_ats: Vec<Instant> = (0..renders.count())
            .map(|call| renders.play_at(call))
            .collect();
        let packets =
            |at: Instant| (at - first).as_nanos() as f64 / PACKET_DURATION.as_nanos() as f64;
        for pair in play_ats.windows(2) {
            let step = packets(pair[1]) - packets(pair[0]);
            assert!(
                step >= 0.999 && (step - step.round()).abs() < 0.001,
                "renders step by {step} packets"
            );
        }
        let after_stall = (packets(play_ats[21]) - packets(play_ats[20])).round() as u16;
        assert!(
            after_stall > 1,
            "packets due during the stall were skipped, not rendered late"
        );
        let (_, first_sent) = &audio[0];
        let first_packet = packets(renders.play_at(call_of(first_sent))).round() as u16;
        for (_, packet) in &audio {
            let packet_index = packets(renders.play_at(call_of(packet))).round() as u16;
            assert_eq!(
                packet.seq.wrapping_sub(first_sent.seq),
                packet_index - first_packet,
                "a sequence number off its packet's place on the timeline"
            );
        }
    }

    #[test]
    fn finishing_with_playout_delay_preserves_the_tail() {
        check_finish_playout_delay(Protocol::AirPlay1);
    }

    pub(crate) fn check_finish_playout_delay(protocol: Protocol) {
        for delay in [Duration::ZERO, Duration::from_millis(200)] {
            let fake = Fake::start(Behavior::default());
            let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
            fake.advertise_airplay(&browser, REALTIME_AIRPLAY_2_RECORD);
            let renders = Renders::default();
            let settings = Settings {
                protocol: Some(protocol),
                answer_timeout: Duration::from_secs(3),
                teardown_timeout: Duration::from_millis(100),
                ..settings()
            };
            let bound = settings.latency + settings.teardown_timeout + delay;
            let stream =
                Stream::start_with(&browser, fake.id(), Ok(settings), renders.renderer(None));
            assert!(wait_until(Duration::from_secs(10), || fake.audio().len() >= 40));
            let finished_at = Instant::now();
            let finishing = stream.finish(delay);
            thread::sleep(if delay.is_zero() {
                Duration::from_millis(100)
            } else {
                LATENCY + Duration::from_millis(50)
            });
            let audio = fake.audio();
            let last = &audio.last().unwrap().1;
            let packet_end = renders.play_at(call_of(last)) + PACKET_DURATION;
            let played_out = packet_end + delay;
            let bytes = fake.audio_bytes().last().unwrap().clone();
            let requested_at = Instant::now();
            if !delay.is_zero() {
                assert!(requested_at >= packet_end);
                assert!(requested_at < played_out);
            }
            fake.request_retransmission(last.seq, 1);
            finishing.wait();
            let returned_at = Instant::now();
            let expected_teardowns = if protocol == Protocol::AirPlay2 { 2 } else { 1 };
            assert!(wait_until(Duration::from_secs(1), || {
                fake.methods().iter().filter(|m| *m == "TEARDOWN").count() == expected_teardowns
            }));
            let teardowns: Vec<_> = fake
                .requests()
                .into_iter()
                .filter(|(_, _, r)| r.method == "TEARDOWN")
                .map(|(at, _, _)| at)
                .collect();
            assert!(
                teardowns[0] >= played_out,
                "{protocol:?}, {delay:?}: TEARDOWN came {:?} early",
                played_out.saturating_duration_since(teardowns[0])
            );
            assert!(teardowns.last().unwrap().duration_since(played_out) < PROMPT);
            assert!(
                returned_at >= played_out,
                "{protocol:?}, {delay:?}: wait returned {:?} early",
                played_out.saturating_duration_since(returned_at)
            );
            assert!(returned_at.duration_since(finished_at) <= bound + PROMPT);
            let resent = match protocol {
                Protocol::AirPlay1 => fake
                    .control()
                    .iter()
                    .any(|b| b[1] == 0xD6 && b[4..] == bytes),
                Protocol::AirPlay2 => {
                    fake.audio_bytes().iter().filter(|b| **b == bytes).count() == 2
                }
            };
            assert!(
                resent,
                "{protocol:?}, {delay:?}: the tail retransmission was not byte-identical"
            );
        }
    }

    #[test]
    fn finishing_plays_out_answers_retransmissions_then_tears_down() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let renders = Renders::default();
        let stream =
            Stream::start_with(&browser, fake.id(), Ok(settings()), renders.renderer(None));
        assert!(wait_until(Duration::from_secs(2), || fake.audio().len() >= 40));
        let finishing = stream.finish(Duration::ZERO);
        let finished_at = Instant::now();
        assert!(!finishing.is_finished());
        let early = fake.audio_bytes()[5].clone();
        thread::sleep(Duration::from_millis(100));
        fake.request_retransmission(u16::from_be_bytes([early[2], early[3]]), 1);
        finishing.wait();
        let waited = finished_at.elapsed();

        let retransmission = fake
            .control()
            .into_iter()
            .find(|b| b[1] == 0xD6)
            .expect("a retransmission");
        assert_eq!(&retransmission[4..], early.as_slice());
        let (teardown_at, _, _) = fake
            .requests()
            .into_iter()
            .find(|(_, _, r)| r.method == "TEARDOWN")
            .unwrap();
        let audio = fake.audio();
        let last = audio.iter().max_by_key(|(at, _)| *at).unwrap();
        let played_out = renders.play_at(call_of(&last.1)) + PACKET_DURATION;
        assert!(
            teardown_at >= played_out,
            "TEARDOWN came {:?} early",
            played_out - teardown_at
        );
        assert!(
            teardown_at - played_out < PROMPT,
            "TEARDOWN came {:?} late",
            teardown_at - played_out
        );
        assert!(
            waited <= LATENCY + settings().teardown_timeout,
            "finishing took {waited:?}"
        );
    }

    #[test]
    fn finishing_plays_out_a_tail_recovered_by_retransmission() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let renders = Renders::default();
        let failing = Arc::new(AtomicBool::new(false));
        let tail_from = 30;
        let settings = Settings {
            first_seq: u32::from(0u16.wrapping_sub(tail_from as u16 + 3)),
            failing_sends: failing.clone(),
            ..settings()
        };
        let mut render = renders.renderer(None);
        let mut calls = 0;
        let fail_tail = failing.clone();
        let stream = Stream::start_with(&browser, fake.id(), Ok(settings), move |r: Render<'_>| {
            fail_tail.store(calls >= tail_from, Ordering::Release);
            calls += 1;
            render(r);
        });
        assert!(wait_until(Duration::from_secs(2), || failing.load(Ordering::Acquire)));
        thread::sleep(Duration::from_millis(100));
        let finishing = stream.finish(Duration::ZERO);
        thread::sleep(Duration::from_millis(20));
        failing.store(false, Ordering::Release);
        let received = fake.audio();
        assert!(received.iter().all(|(_, p)| call_of(p) < tail_from));
        let last_received = received.last().unwrap().1.seq;
        let older = received[5].1.seq;
        fake.request_retransmission(last_received.wrapping_add(1), 30);
        fake.request_retransmission(older, 1);
        finishing.wait();

        let recovered: Vec<AudioPacket> = fake
            .control()
            .iter()
            .filter(|b| b[1] == 0xD6)
            .map(|b| decode_audio(&b[4..]))
            .collect();
        assert!(recovered.iter().any(|p| p.seq == older));
        let tail: Vec<&AudioPacket> = recovered.iter().filter(|p| p.seq != older).collect();
        assert!(tail.len() >= 5, "recovered {} tail packets", tail.len());
        assert_eq!(call_of(tail[0]), tail_from);
        for pair in tail.windows(2) {
            assert_eq!(pair[1].seq, pair[0].seq.wrapping_add(1));
        }
        assert!(tail.iter().any(|p| p.seq == 0), "the tail crosses the wrap");
        let played_out = renders.play_at(call_of(tail[tail.len() - 1])) + PACKET_DURATION;
        let (teardown_at, _, _) = fake
            .requests()
            .into_iter()
            .find(|(_, _, r)| r.method == "TEARDOWN")
            .unwrap();
        assert!(
            teardown_at >= played_out,
            "TEARDOWN came {:?} before the recovered tail played out",
            played_out - teardown_at
        );
        assert!(teardown_at - played_out < PROMPT);
    }

    #[test]
    fn finishing_against_a_silent_receiver_is_bounded() {
        let fake = Fake::start(Behavior {
            rtsp: Rtsp::SilentAfterRecord,
            ..Behavior::default()
        });
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || fake.audio().len() >= 20));
        let started = Instant::now();
        stream.finish(Duration::ZERO).wait();
        let waited = started.elapsed();
        assert!(
            waited <= LATENCY + settings().teardown_timeout + Duration::from_millis(150),
            "took {waited:?}"
        );
        assert!(fake.methods().contains(&"TEARDOWN".to_owned()));
        assert!(wait_until(Duration::from_secs(1), || fake
            .closed(0)
            .is_some()));
    }

    #[test]
    fn audio_that_keeps_failing_to_send_loses_the_stream() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let failing = Arc::new(AtomicBool::new(false));
        let settings = Settings {
            send_failure_limit: Duration::from_millis(500),
            failing_sends: failing.clone(),
            ..settings()
        };
        let limit = settings.send_failure_limit;
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || fake.audio().len() >= 20));

        failing.store(true, Ordering::Release);
        thread::sleep(limit / 2);
        failing.store(false, Ordering::Release);
        let sent = fake.audio().len();
        assert!(wait_until(Duration::from_secs(1), || fake.audio().len() > sent + 10));
        assert!(streaming(&stream), "{:?}", stream.status());

        failing.store(true, Ordering::Release);
        let failing_since = Instant::now();
        assert!(wait_until(limit + Duration::from_secs(1), || !streaming(
            &stream
        )));
        let lost_after = failing_since.elapsed();
        let Status::Lost(reason) = stream.status() else {
            panic!("{:?}", stream.status())
        };
        assert!(reason.contains("injected failure"), "{reason}");
        assert!(
            lost_after >= limit && lost_after < limit + PROMPT,
            "lost after {lost_after:?}"
        );
        assert!(wait_until(Duration::from_secs(1), || fake
            .closed(0)
            .is_some()));
        assert!(fake.methods().contains(&"TEARDOWN".to_owned()));
    }

    #[test]
    fn a_panicking_renderer_loses_the_stream_and_frees_the_receiver() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let mut calls = 0;
        let stream =
            Stream::start_with(&browser, fake.id(), Ok(settings()), move |_: Render<'_>| {
                calls += 1;
                assert!(calls < 20, "renderer failed on purpose");
            });
        let next = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || matches!(
            stream.status(),
            Status::Lost(_)
        )));
        let Status::Lost(reason) = stream.status() else {
            unreachable!()
        };
        assert!(reason.starts_with("the renderer panicked"), "{reason}");
        assert!(reason.contains("renderer failed on purpose"), "{reason}");
        assert!(wait_until(Duration::from_secs(1), || fake
            .requests()
            .iter()
            .any(
                |(_, connection, r)| *connection == 0 && r.method == "TEARDOWN"
            )));
        assert!(wait_until(Duration::from_secs(2), || streaming(&next)));
    }

    #[test]
    fn finishing_waits_out_a_blocked_renderer_and_the_receiver_stays_taken_meanwhile() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let (release, held) = mpsc::channel::<()>();
        let (blocked, blocking) = mpsc::channel::<()>();
        let mut calls = 0;
        let stream =
            Stream::start_with(&browser, fake.id(), Ok(settings()), move |_: Render<'_>| {
                calls += 1;
                if calls == 20 {
                    let _ = blocked.send(());
                    let _ = held.recv();
                }
            });
        blocking.recv_timeout(Duration::from_secs(2)).unwrap();
        let next = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );

        let bound = LATENCY + settings().teardown_timeout;
        let (waited, wait_returned) = mpsc::channel();
        let finishing = thread::spawn(move || {
            stream.finish(Duration::ZERO).wait();
            let _ = waited.send(Instant::now());
        });
        let early = wait_returned.recv_timeout(bound + PROMPT);
        let taken = next.status() == Status::Connecting;
        let released = Instant::now();
        drop(release);
        let returned = wait_returned
            .recv_timeout(bound + PROMPT)
            .expect("finishing never returned");
        finishing.join().unwrap();
        assert!(
            early.is_err(),
            "finishing returned while the session still ran"
        );
        assert!(taken, "the receiver was freed while the session still ran");
        let after = returned.duration_since(released);
        assert!(
            after < bound + PROMPT,
            "returned {after:?} after the renderer"
        );
        assert!(wait_until(Duration::from_secs(2), || streaming(&next)));
    }

    #[test]
    fn an_audio_latency_not_below_the_streams_is_refused_after_teardown() {
        let latency_frames = crate::clock::frames(LATENCY);
        for audio_latency in [latency_frames - 1, latency_frames, latency_frames + 1] {
            let fake = Fake::start(Behavior {
                audio_latency: Some(audio_latency),
                ..Behavior::default()
            });
            let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
            let stream = Stream::start_with(
                &browser,
                fake.id(),
                Ok(settings()),
                Renders::default().renderer(None),
            );
            assert!(wait_until(Duration::from_secs(2), || stream.status()
                != Status::Connecting));
            if audio_latency < latency_frames {
                assert!(streaming(&stream), "{:?}", stream.status());
                continue;
            }
            let Status::Unreachable(reason) = stream.status() else {
                panic!("{audio_latency} frames: {:?}", stream.status())
            };
            assert!(reason.starts_with("fake: "), "{reason}");
            assert!(
                reason.contains(&format!("{audio_latency} frames")),
                "{reason}"
            );
            assert_eq!(fake.methods().last().map(String::as_str), Some("TEARDOWN"));
            assert!(fake.audio().is_empty());
        }
    }

    #[test]
    fn a_stopped_stream_releases_its_session_within_its_bound() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || fake.audio().len() >= 20));
        let stopped_at = Instant::now();
        let stopping = stream.stop();
        let sent = fake.audio().len();
        stopping.wait();
        let bound = settings().teardown_timeout + Duration::from_millis(40);
        assert!(stopped_at.elapsed() <= bound, "{:?}", stopped_at.elapsed());
        assert!(fake.methods().iter().any(|method| method == "TEARDOWN"));
        std::thread::sleep(Duration::from_millis(50));
        assert!(fake.audio().len() <= sent + 1);
    }

    /// Claude: an asleep fake drops each SYN, so the attempt is pending; once
    /// it wakes, an attempt still pending would connect at its next SYN.
    #[test]
    fn a_stream_stopped_while_its_connection_is_pending_ends_at_once_and_never_connects() {
        let fake = Fake::asleep(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let pending = Settings {
            connect_timeout: Duration::from_secs(5),
            wake_window: Duration::from_secs(10),
            ..settings()
        };
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(pending),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(300));
        assert_eq!(stream.status(), Status::Connecting);
        let stopped_at = Instant::now();
        stream.stop().wait();
        assert!(stopped_at.elapsed() < PROMPT, "{:?}", stopped_at.elapsed());
        fake.wake();
        thread::sleep(Duration::from_millis(2500));
        assert_eq!(fake.accepted(), 0, "a stopped stream's attempt connected");
    }

    #[test]
    fn a_stream_stopped_while_finding_its_receiver_ends_at_once() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let elsewhere: ReceiverId = "02:00:00:00:00:01".parse().unwrap();
        let stream = Stream::start_with(
            &browser,
            elsewhere,
            Ok(settings()),
            Renders::default().renderer(None),
        );
        std::thread::sleep(Duration::from_millis(50));
        let stopped_at = Instant::now();
        stream.stop().wait();
        assert!(stopped_at.elapsed() < Duration::from_millis(100));
        assert!(fake.requests().is_empty());
    }

    #[test]
    fn dropping_a_stream_tears_down_at_once() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || fake.audio().len() >= 20));
        let dropped_at = Instant::now();
        drop(stream);
        assert!(dropped_at.elapsed() < Duration::from_millis(20));
        assert!(wait_until(Duration::from_millis(500), || fake
            .closed(0)
            .is_some()));
        let (teardown_at, _, _) = fake
            .requests()
            .into_iter()
            .find(|(_, _, r)| r.method == "TEARDOWN")
            .unwrap();
        assert!(
            teardown_at - dropped_at < PROMPT,
            "TEARDOWN after {:?}",
            teardown_at - dropped_at
        );
    }

    #[test]
    fn a_second_stream_waits_for_the_first_to_tear_down() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let first = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || fake.audio().len() >= 10));
        let second = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(200));
        assert_eq!(second.status(), Status::Connecting);
        assert!(
            fake.requests()
                .iter()
                .all(|(_, connection, _)| *connection == 0)
        );
        first.finish(Duration::ZERO).wait();
        assert!(wait_until(Duration::from_secs(2), || streaming(&second)));

        let requests = fake.requests();
        let (teardown_at, _, _) = requests
            .iter()
            .find(|(_, _, r)| r.method == "TEARDOWN")
            .unwrap();
        let (second_at, _, request) = requests
            .iter()
            .find(|(_, connection, _)| *connection == 1)
            .unwrap();
        assert_eq!(request.method, "OPTIONS");
        assert!(second_at > teardown_at);
    }

    #[test]
    fn an_unknown_receiver_is_unreachable_after_the_lookup_window() {
        let browser = Browser::offline();
        let started = Instant::now();
        let stream = Stream::start_with(
            &browser,
            ID.parse().unwrap(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || stream.status()
            != Status::Connecting));
        let waited = started.elapsed();
        let Status::Unreachable(reason) = stream.status() else {
            panic!("{:?}", stream.status())
        };
        assert!(
            reason.contains("no AirPlay receiver AA:BB:CC:DD:EE:FF was found"),
            "{reason}"
        );
        assert!(waited >= settings().lookup_window && waited < settings().lookup_window + PROMPT);
    }

    #[test]
    fn a_receiver_waits_for_its_raop_service_within_the_lookup_window() {
        let fake = Fake::start(Behavior::default());
        let browser = Browser::offline();
        fake.advertise_airplay(&browser, "deviceid=AA:BB:CC:DD:EE:FF model=Fake flags=0x4");
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(100));
        fake.advertise(&browser, SHAIRPORT_SYNC_RAOP);
        assert!(
            wait_until(Duration::from_secs(2), || streaming(&stream)),
            "{:?}",
            stream.status()
        );

        let browser = Browser::offline();
        fake.advertise_airplay(&browser, "deviceid=AA:BB:CC:DD:EE:FF model=Fake flags=0x4");
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        thread::sleep(settings().lookup_window / 2);
        assert_eq!(stream.status(), Status::Connecting);
        assert!(wait_until(Duration::from_secs(1), || stream.status()
            != Status::Connecting));
        let Status::Unreachable(reason) = stream.status() else {
            panic!("{:?}", stream.status())
        };
        assert!(
            reason.starts_with("fake: it does not take AirPlay 2 audio"),
            "{reason}"
        );
    }

    #[test]
    fn a_receiver_found_late_is_still_reached() {
        let fake = Fake::start(Behavior::default());
        let browser = Browser::offline();
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(200));
        fake.advertise(&browser, SHAIRPORT_SYNC_RAOP);
        assert!(wait_until(Duration::from_secs(2), || streaming(&stream)));
    }

    #[test]
    fn finishing_or_dropping_while_connecting_is_prompt() {
        let browser = Browser::offline();
        let looking_up = Settings {
            lookup_window: Duration::from_secs(10),
            ..settings()
        };
        let stream = Stream::start_with(
            &browser,
            ID.parse().unwrap(),
            Ok(looking_up.clone()),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        stream.finish(Duration::ZERO).wait();
        assert!(
            started.elapsed() < PROMPT,
            "lookup cancelled after {:?}",
            started.elapsed()
        );

        let fake = Fake::start(Behavior::default());
        let browser = Browser::offline();
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(looking_up),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(50));
        drop(stream);
        fake.advertise(&browser, SHAIRPORT_SYNC_RAOP);
        thread::sleep(Duration::from_millis(200));
        assert!(
            fake.seen().is_empty(),
            "a stream dropped while looking up its receiver connected to it"
        );

        let fake = Fake::start(Behavior {
            rtsp: Rtsp::HangAt(Step::Announce),
            ..Behavior::default()
        });
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let hanging = || Settings {
            answer_timeout: Duration::from_secs(5),
            ..settings()
        };
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(hanging()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(1), || fake
            .methods()
            .contains(&"ANNOUNCE".to_owned())));
        let started = Instant::now();
        stream.finish(Duration::ZERO).wait();
        assert!(
            started.elapsed() < PROMPT,
            "handshake cancelled after {:?}",
            started.elapsed()
        );

        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(hanging()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(1), || fake
            .requests()
            .iter()
            .any(|(_, c, r)| *c == 1 && r.method == "ANNOUNCE")));
        let dropped_at = Instant::now();
        drop(stream);
        assert!(wait_until(Duration::from_secs(1), || fake
            .closed(1)
            .is_some()));
        assert!(fake.closed(1).unwrap() - dropped_at < PROMPT);

        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let first = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || streaming(&first)));
        let waiting = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        waiting.finish(Duration::ZERO).wait();
        assert!(
            started.elapsed() < PROMPT,
            "turn wait cancelled after {:?}",
            started.elapsed()
        );
        let dropped = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(50));
        drop(dropped);
        first.finish(Duration::ZERO).wait();
        thread::sleep(Duration::from_millis(200));
        assert!(
            fake.requests()
                .iter()
                .all(|(_, connection, _)| *connection == 0),
            "a cancelled stream connected"
        );

        let asleep = Fake::asleep(Behavior::default());
        let browser = asleep.browser(SHAIRPORT_SYNC_RAOP);
        let waking = |connect_timeout| Settings {
            connect_timeout,
            wake_window: Duration::from_secs(10),
            ..settings()
        };
        let between_attempts = waking(Duration::from_millis(100));
        let stream = Stream::start_with(
            &browser,
            asleep.id(),
            Ok(between_attempts),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(300));
        assert_eq!(stream.status(), Status::Connecting);
        let started = Instant::now();
        stream.finish(Duration::ZERO).wait();
        assert!(
            started.elapsed() < PROMPT,
            "retries cancelled after {:?}",
            started.elapsed()
        );
        let during_an_attempt = waking(Duration::from_secs(1));
        let stream = Stream::start_with(
            &browser,
            asleep.id(),
            Ok(during_an_attempt),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(300));
        assert_eq!(stream.status(), Status::Connecting);
        drop(stream);
        asleep.wake();
        thread::sleep(Duration::from_millis(700));
        assert!(
            asleep.requests().is_empty(),
            "a dropped stream kept retrying"
        );
    }

    #[test]
    fn auth_setup_is_sent_only_after_an_mfi_receiver_refuses() {
        let fake = Fake::start(Behavior {
            auth_setup_required: true,
            ..Behavior::default()
        });
        let browser = fake.browser(MOVE_2_RAOP);
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || streaming(&stream)));
        let requests: Vec<(usize, String)> = fake
            .requests()
            .into_iter()
            .map(|(_, c, r)| (c, r.method))
            .collect();
        let expected = [
            (0, "OPTIONS"),
            (0, "ANNOUNCE"),
            (1, "OPTIONS"),
            (1, "POST"),
            (1, "ANNOUNCE"),
            (1, "SETUP"),
            (1, "RECORD"),
        ];
        assert_eq!(requests, expected.map(|(c, m)| (c, m.to_owned())));
        let post = fake
            .requests()
            .into_iter()
            .find(|(_, _, r)| r.method == "POST")
            .unwrap()
            .2;
        assert_eq!(post.uri, "/auth-setup");
        assert_eq!(
            post.header("Content-Type"),
            Some("application/octet-stream")
        );
        assert_eq!((post.body.len(), post.body[0]), (33, 0x01));

        let fake = Fake::start(Behavior {
            auth_setup_required: true,
            ..Behavior::default()
        });
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || stream.status()
            != Status::Connecting));
        assert_eq!(
            stream.status(),
            Status::Unreachable("fake: it refused ANNOUNCE with 403 Forbidden".into())
        );

        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(MOVE_2_RAOP);
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || streaming(&stream)));
        assert_eq!(fake.methods(), ["OPTIONS", "ANNOUNCE", "SETUP", "RECORD"]);
    }

    #[test]
    fn unsupported_receivers_say_why_without_being_contacted() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser("cn=0,1 et=1 tp=UDP");
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(1), || stream.status()
            != Status::Connecting));
        assert_eq!(
            stream.status(),
            Status::Unreachable("fake: it accepts only encrypted audio (et=1)".into())
        );
        assert!(fake.requests().is_empty());
    }

    #[test]
    fn a_receiver_waking_from_standby_is_reached_once_it_answers() {
        let fake = Fake::asleep(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let waking = Settings {
            connect_timeout: Duration::from_millis(100),
            wake_window: Duration::from_secs(3),
            ..settings()
        };
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(waking),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(700));
        assert_eq!(stream.status(), Status::Connecting);
        let woke_at = Instant::now();
        fake.wake();
        assert!(
            wait_until(Duration::from_secs(2), || streaming(&stream)),
            "{:?}",
            stream.status()
        );
        let (connected_at, _, _) = fake.requests()[0].clone();
        assert!(connected_at - woke_at < Duration::from_secs(1));
    }

    #[test]
    fn a_receiver_that_stays_asleep_is_unreachable_after_the_wake_window_with_its_last_error() {
        let first = Fake::asleep(Behavior::default());
        let browser = first.browser(SHAIRPORT_SYNC_RAOP);
        let started = Instant::now();
        let asleep = Settings {
            connect_timeout: Duration::from_millis(100),
            ..settings()
        };
        let window = asleep.wake_window;
        let stream = Stream::start_with(
            &browser,
            first.id(),
            Ok(asleep),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(300));
        let moved = Fake::asleep(Behavior::default());
        moved.advertise(&browser, SHAIRPORT_SYNC_RAOP);
        assert!(wait_until(window + Duration::from_secs(1), || stream
            .status()
            != Status::Connecting));
        let waited = started.elapsed();
        let Status::Unreachable(reason) = stream.status() else {
            panic!("{:?}", stream.status())
        };
        assert!(
            reason
                == format!(
                    "fake: could not connect to {} (connection timed out)",
                    moved.address
                ),
            "{reason}"
        );
        assert!(
            waited >= window && waited < window + PROMPT,
            "unreachable after {waited:?}"
        );

        let refusing = Fake::start(Behavior {
            auth_setup_required: true,
            ..Behavior::default()
        });
        let browser = refusing.browser(SHAIRPORT_SYNC_RAOP);
        let started = Instant::now();
        let stream = Stream::start_with(
            &browser,
            refusing.id(),
            Ok(Settings {
                wake_window: Duration::from_secs(10),
                ..settings()
            }),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || stream.status()
            != Status::Connecting));
        assert!(
            started.elapsed() < PROMPT,
            "a refused handshake took {:?}",
            started.elapsed()
        );
        assert_eq!(
            stream.status(),
            Status::Unreachable("fake: it refused ANNOUNCE with 403 Forbidden".into())
        );
        thread::sleep(Duration::from_millis(600));
        assert!(
            fake_connections(&refusing) == [0],
            "a refused handshake was retried"
        );
    }

    const REQUIRES_A_PIN: &str =
        "fake: it requires pairing with a PIN; allow AirPlay without a PIN in its settings";

    fn with_a_pin() -> String {
        SHAIRPORT_SYNC_RAOP.replace("sf=0x4", "sf=0xC")
    }

    #[test]
    fn a_queued_stream_to_a_receiver_that_now_requires_a_pin_never_connects() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let first = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || streaming(&first)));
        let queued = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(100));
        assert_eq!(queued.status(), Status::Connecting);
        fake.advertise(&browser, &with_a_pin());
        first.finish(Duration::ZERO).wait();
        assert!(wait_until(Duration::from_secs(2), || queued.status()
            != Status::Connecting));
        assert_eq!(queued.status(), Status::Unreachable(REQUIRES_A_PIN.into()));
        thread::sleep(Duration::from_millis(200));
        assert_eq!(fake.accepted(), 1, "the queued stream connected");
    }

    #[test]
    fn a_queued_stream_still_tries_a_receiver_or_service_gone_from_browsing() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let first = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || streaming(&first)));
        let queued = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(100));
        fake.withdraw(&browser, 1);
        first.finish(Duration::ZERO).wait();
        assert!(
            wait_until(Duration::from_secs(2), || streaming(&queued)),
            "a receiver gone from browsing is still tried as it was found: {:?}",
            queued.status()
        );
        drop(queued);

        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        fake.advertise_airplay(&browser, REALTIME_AIRPLAY_2_RECORD);
        let airplay_1 = || Settings {
            protocol: Some(Protocol::AirPlay1),
            ..settings()
        };
        let first = Stream::start_with(
            &browser,
            fake.id(),
            Ok(airplay_1()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || streaming(&first)));
        let queued = Stream::start_with(
            &browser,
            fake.id(),
            Ok(airplay_1()),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(100));
        fake.withdraw(&browser, 1);
        first.finish(Duration::ZERO).wait();
        assert!(
            wait_until(Duration::from_secs(2), || streaming(&queued)),
            "a service gone from browsing is still tried as it was found: {:?}",
            queued.status()
        );
    }

    #[test]
    fn a_waking_receiver_that_now_requires_a_pin_ends_its_retries_without_connecting() {
        let fake = Fake::asleep(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let waking = Settings {
            connect_timeout: Duration::from_millis(100),
            wake_window: Duration::from_secs(3),
            ..settings()
        };
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(waking),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(300));
        assert_eq!(stream.status(), Status::Connecting);
        fake.advertise(&browser, &with_a_pin());
        fake.wake();
        assert!(wait_until(Duration::from_secs(2), || stream.status()
            != Status::Connecting));
        assert_eq!(stream.status(), Status::Unreachable(REQUIRES_A_PIN.into()));
        thread::sleep(Duration::from_millis(200));
        assert_eq!(fake.accepted(), 0, "the waking stream connected");
    }

    #[test]
    fn the_wake_window_bounds_every_attempt_across_addresses() {
        let asleep: Vec<Fake> = (0..3).map(|_| Fake::asleep(Behavior::default())).collect();
        let browser = Browser::offline();
        for (interface, fake) in (1..).zip(&asleep) {
            fake.advertise_on(&browser, interface, SHAIRPORT_SYNC_RAOP);
        }
        let several = Settings {
            connect_timeout: Duration::from_millis(400),
            wake_window: Duration::from_millis(500),
            ..settings()
        };
        let window = several.wake_window;
        let started = Instant::now();
        let stream = Stream::start_with(
            &browser,
            asleep[0].id(),
            Ok(several),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(3), || stream.status()
            != Status::Connecting));
        let waited = started.elapsed();
        let Status::Unreachable(reason) = stream.status() else {
            panic!("{:?}", stream.status())
        };
        assert!(waited < window + PROMPT, "unreachable after {waited:?}");
        assert!(
            reason.starts_with(&format!(
                "fake: could not connect to {} (connection timed out)",
                asleep[0].address
            )),
            "{reason}"
        );

        let (stays, wakes) = (
            Fake::asleep(Behavior::default()),
            Fake::asleep(Behavior::default()),
        );
        let browser = Browser::offline();
        stays.advertise_on(&browser, 1, SHAIRPORT_SYNC_RAOP);
        wakes.advertise_on(&browser, 2, SHAIRPORT_SYNC_RAOP);
        let short = Settings {
            connect_timeout: Duration::from_millis(400),
            wake_window: Duration::from_millis(300),
            ..settings()
        };
        let window = short.wake_window;
        let started = Instant::now();
        let stream = Stream::start_with(
            &browser,
            stays.id(),
            Ok(short),
            Renders::default().renderer(None),
        );
        thread::sleep((started + window + Duration::from_millis(50)) - Instant::now());
        wakes.wake();
        thread::sleep(Duration::from_millis(600));
        assert!(
            matches!(stream.status(), Status::Unreachable(_)),
            "{:?}",
            stream.status()
        );
        assert_eq!(
            wakes.accepted(),
            0,
            "an attempt began after the wake window"
        );
    }

    #[test]
    fn a_connection_cancelled_during_one_address_tries_no_further_address() {
        let refused = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|listener| listener.local_addr())
            .unwrap();
        let fake = Fake::start(Behavior::default());
        let checks = AtomicUsize::new(0);
        let cancelled_during_the_first_attempt = || checks.fetch_add(1, Ordering::SeqCst) > 0;
        let opened = Connection::open(
            &[refused, fake.address],
            Duration::from_secs(1),
            Instant::now() + Duration::from_secs(1),
            vec![],
            &cancelled_during_the_first_attempt,
        );
        assert!(
            matches!(opened, Err(OpenError::Stopped)),
            "{:?}",
            opened
                .as_ref()
                .map(Connection::peer_addr)
                .map_err(ToString::to_string)
        );
        thread::sleep(Duration::from_millis(100));
        assert_eq!(fake.accepted(), 0, "a cancelled attempt connected");
    }

    fn fake_connections(fake: &Fake) -> Vec<usize> {
        let mut connections: Vec<usize> = fake.requests().iter().map(|(_, c, _)| *c).collect();
        connections.dedup();
        connections
    }

    #[test]
    fn a_closed_connection_or_timing_silence_loses_the_stream() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || streaming(&stream)));
        fake.close_connections();
        assert!(wait_until(Duration::from_secs(1), || !streaming(&stream)));
        assert_eq!(
            stream.status(),
            Status::Lost("the receiver closed the RTSP connection".into())
        );

        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let quiet = Settings {
            timing_silence: Duration::from_millis(300),
            ..settings()
        };
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(quiet),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || streaming(&stream)));
        thread::sleep(Duration::from_millis(400));
        assert!(streaming(&stream), "no timing requests yet is not silence");
        fake.time(Ntp(1));
        assert!(wait_until(Duration::from_millis(500), || !streaming(
            &stream
        )));
        assert_eq!(
            stream.status(),
            Status::Lost("it stopped requesting timing".into())
        );
    }
}
