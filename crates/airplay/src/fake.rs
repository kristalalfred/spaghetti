use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::browse::{Event, InstanceKey, Kind};
use crate::clock::{Clock, Ntp};
use crate::receiver::tests::txt;
use crate::{Browser, ReceiverId, SAMPLE_RATE};

const ID: &str = "AA:BB:CC:DD:EE:FF";
const INSTANCE: &str = "AABBCCDDEEFF@fake";
const POLL: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Rtsp {
    #[default]
    Answer,
    HangAtAnnounce,
    SilentAfterRecord,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Behavior {
    pub(crate) auth_setup_required: bool,
    pub(crate) audio_latency: Option<u32>,
    pub(crate) rtsp: Rtsp,
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
}

/// Claude: a minimal AirPlay 1 receiver on localhost that records everything
/// a stream sends it and answers as `Behavior` says.
pub(crate) struct Fake {
    address: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    client: Arc<Mutex<Option<(u16, u16)>>>,
    control: UdpSocket,
    timing: UdpSocket,
    connections: Arc<Mutex<Vec<TcpStream>>>,
    stop: Arc<AtomicBool>,
}

impl Fake {
    pub(crate) fn start(behavior: Behavior) -> Fake {
        let localhost = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let listener = TcpListener::bind(localhost).unwrap();
        listener.set_nonblocking(true).unwrap();
        let audio = UdpSocket::bind(localhost).unwrap();
        let control = UdpSocket::bind(localhost).unwrap();
        let timing = UdpSocket::bind(localhost).unwrap();
        for socket in [&audio, &control, &timing] {
            socket
                .set_read_timeout(Some(Duration::from_millis(20)))
                .unwrap();
        }
        let ports = (
            audio.local_addr().unwrap().port(),
            control.local_addr().unwrap().port(),
            timing.local_addr().unwrap().port(),
        );
        let fake = Fake {
            address: listener.local_addr().unwrap(),
            seen: Arc::default(),
            client: Arc::default(),
            control: control.try_clone().unwrap(),
            timing,
            connections: Arc::default(),
            stop: Arc::default(),
        };
        for (socket, audio) in [(audio, true), (control, false)] {
            let (seen, stop) = (fake.seen.clone(), fake.stop.clone());
            thread::spawn(move || {
                let mut buf = [0u8; 2048];
                while !stop.load(Ordering::Acquire) {
                    if let Ok(len) = socket.recv(&mut buf) {
                        let (at, bytes) = (Instant::now(), buf[..len].to_vec());
                        let seen_now = if audio {
                            Seen::Audio { at, bytes }
                        } else {
                            Seen::Control { bytes }
                        };
                        seen.lock().unwrap().push(seen_now);
                    }
                }
            });
        }
        let (seen, client, connections, stop) = (
            fake.seen.clone(),
            fake.client.clone(),
            fake.connections.clone(),
            fake.stop.clone(),
        );
        thread::spawn(move || {
            let mut count = 0;
            while !stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        connections
                            .lock()
                            .unwrap()
                            .push(stream.try_clone().unwrap());
                        let (seen, client, stop) = (seen.clone(), client.clone(), stop.clone());
                        let connection = count;
                        count += 1;
                        thread::spawn(move || {
                            serve(stream, connection, behavior, ports, &seen, &client, &stop);
                        });
                    }
                    Err(_) => thread::sleep(POLL),
                }
            }
        });
        fake
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
        self.advertise_as(browser, Kind::Raop, INSTANCE, record);
    }

    /// Claude: advertises only the AirPlay 2 service, as a receiver does whose
    /// `_airplay._tcp` record resolves before its `_raop._tcp` one.
    pub(crate) fn advertise_airplay(&self, browser: &Browser) {
        let record = format!("deviceid={ID} model=Fake flags=0x4");
        self.advertise_as(browser, Kind::AirPlay, "fake", &record);
    }

    fn advertise_as(&self, browser: &Browser, kind: Kind, name: &str, record: &str) {
        let key = InstanceKey {
            kind,
            name: name.into(),
            interface: 1,
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
        self.seen.lock().unwrap().clone()
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

    pub(crate) fn audio(&self) -> Vec<(Instant, AudioPacket)> {
        self.seen()
            .into_iter()
            .filter_map(|seen| match seen {
                Seen::Audio { at, bytes } => Some((at, decode_audio(&bytes))),
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

    fn client(&self, which: fn((u16, u16)) -> u16) -> SocketAddr {
        let ports = self
            .client
            .lock()
            .unwrap()
            .expect("the stream has sent SETUP");
        SocketAddr::from((Ipv4Addr::LOCALHOST, which(ports)))
    }

    /// Claude: sends a timing request saying it was sent at `sent`, and returns
    /// the reply with the instants around the exchange.
    pub(crate) fn time(&self, sent: Ntp) -> (Instant, [u8; 32], Instant) {
        let mut request = [0u8; 32];
        request[..4].copy_from_slice(&[0x80, 0xD2, 0x00, 0x07]);
        request[24..32].copy_from_slice(&sent.to_be_bytes());
        let before = Instant::now();
        self.timing
            .send_to(&request, self.client(|(_, timing)| timing))
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
            .send_to(&request, self.client(|(control, _)| control))
            .unwrap();
    }

    pub(crate) fn close_connections(&self) {
        for connection in self.connections.lock().unwrap().iter() {
            let _ = connection.shutdown(Shutdown::Both);
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.close_connections();
    }
}

fn serve(
    mut stream: TcpStream,
    connection: usize,
    behavior: Behavior,
    (audio, control, timing): (u16, u16, u16),
    seen: &Mutex<Vec<Seen>>,
    client: &Mutex<Option<(u16, u16)>>,
    stop: &AtomicBool,
) {
    stream
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let mut received = Vec::new();
    let mut auth_setup = false;
    let mut silent = false;
    loop {
        let request = loop {
            if let Some((request, len)) = parse_request(&received) {
                received.drain(..len);
                break Some(request);
            }
            let mut chunk = [0u8; 4096];
            match stream.read(&mut chunk) {
                Ok(0) => break None,
                Ok(n) => received.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    if stop.load(Ordering::Acquire) {
                        break None;
                    }
                }
                Err(_) => break None,
            }
        };
        let Some(request) = request else {
            seen.lock().unwrap().push(Seen::Closed {
                at: Instant::now(),
                connection,
            });
            return;
        };
        seen.lock().unwrap().push(Seen::Request {
            at: Instant::now(),
            connection,
            request: request.clone(),
        });
        if silent {
            continue;
        }
        let cseq = request.header("CSeq").unwrap_or("0").to_owned();
        let mut headers = String::new();
        let mut body = Vec::new();
        let status = match request.method.as_str() {
            "OPTIONS" => {
                headers.push_str(
                    "Public: ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, POST\r\n",
                );
                "200 OK"
            }
            "POST" if request.uri == "/auth-setup" => {
                auth_setup = true;
                body = vec![0x42; 32];
                "200 OK"
            }
            "ANNOUNCE" if behavior.rtsp == Rtsp::HangAtAnnounce => continue,
            "ANNOUNCE" if behavior.auth_setup_required && !auth_setup => "403 Forbidden",
            "SETUP" => {
                let transport = request.header("Transport").unwrap_or_default();
                let port = |key: &str| {
                    transport
                        .split(';')
                        .find_map(|part| part.strip_prefix(key)?.strip_prefix('='))
                        .and_then(|p| p.parse().ok())
                        .unwrap()
                };
                *client.lock().unwrap() = Some((port("control_port"), port("timing_port")));
                headers.push_str(&format!(
                    "Session: DEADBEEF;timeout=60\r\nTransport: RTP/AVP/UDP;unicast;mode=record;\
                     server_port={audio};control_port={control};timing_port={timing}\r\n"
                ));
                "200 OK"
            }
            "RECORD" => {
                if let Some(latency) = behavior.audio_latency {
                    headers.push_str(&format!("Audio-Latency: {latency}\r\n"));
                }
                silent = behavior.rtsp == Rtsp::SilentAfterRecord;
                "200 OK"
            }
            _ => "200 OK",
        };
        let mut response =
            format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\nServer: fake\r\n{headers}Content-Length: {}\r\n\r\n", body.len())
                .into_bytes();
        response.extend_from_slice(&body);
        if stream.write_all(&response).is_err() {
            return;
        }
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
    pub(crate) frames: Vec<[i16; 2]>,
}

/// Claude: reads an RTP packet carrying an uncompressed ALAC frame, checking
/// every field of the frame's layout.
pub(crate) fn decode_audio(bytes: &[u8]) -> AudioPacket {
    assert_eq!(bytes[0], 0x80);
    assert_eq!(bytes[1] & 0x7F, 0x60);
    let mut bits = BitReader {
        bytes: &bytes[12..],
        bit: 0,
    };
    assert_eq!(bits.take(3), 1, "channel pair element");
    assert_eq!(bits.take(4), 0, "element instance");
    assert_eq!(bits.take(12), 0, "unused");
    assert_eq!(bits.take(1), 1, "has size");
    assert_eq!(bits.take(2), 0, "shift");
    assert_eq!(bits.take(1), 1, "not compressed");
    let count = bits.take(32) as usize;
    let frames = (0..count)
        .map(|_| [bits.take(16) as u16 as i16, bits.take(16) as u16 as i16])
        .collect();
    assert_eq!(bits.take(3), 7, "end element");
    assert_eq!(bits.bit.div_ceil(8), bytes.len() - 12, "no trailing bytes");
    AudioPacket {
        seq: u16::from_be_bytes([bytes[2], bytes[3]]),
        timestamp: u32::from_be_bytes(bytes[4..8].try_into().unwrap()),
        marker: bytes[1] & 0x80 != 0,
        frames,
    }
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
mod tests {
    use super::*;
    use crate::clock::PACKET_DURATION;
    use crate::receiver::tests::{MOVE_2_RAOP, SHAIRPORT_SYNC_RAOP};
    use crate::stream::Settings;
    use crate::{FRAMES_PER_PACKET, Render, Status, Stream};
    use std::sync::mpsc;

    const LATENCY: Duration = Duration::from_millis(300);
    /// Claude: how soon a cancelled stream lets go, well short of the play-out,
    /// lookup and answer timeouts it would otherwise wait for.
    const PROMPT: Duration = Duration::from_millis(250);

    fn settings() -> Settings {
        Settings {
            latency: LATENCY,
            lookup_window: Duration::from_millis(500),
            connect_timeout: Duration::from_secs(1),
            answer_timeout: Duration::from_secs(1),
            teardown_timeout: Duration::from_millis(300),
            ..Settings::new().unwrap()
        }
    }

    fn expected_frame(n: usize, i: usize) -> [i16; 2] {
        if i == 0 {
            return [n as i16, -1];
        }
        let value = (n * 1009 + i * 31) % 65_536;
        [value as u16 as i16, !(value as u16) as i16]
    }

    /// Claude: what a stream asked its renderer for, call by call. Each packet
    /// carries its call number in its first frame.
    #[derive(Clone, Default)]
    struct Renders(Arc<Mutex<Vec<Instant>>>);

    impl Renders {
        fn renderer(
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

        fn play_at(&self, call: usize) -> Instant {
            self.0.lock().unwrap()[call]
        }

        fn count(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    fn call_of(packet: &AudioPacket) -> usize {
        let call = packet.frames[0][0] as u16 as usize;
        for (i, frame) in packet.frames.iter().enumerate() {
            assert_eq!(*frame, expected_frame(call, i), "frame {i} of call {call}");
        }
        call
    }

    fn streaming(stream: &Stream) -> bool {
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
        stream.finish().wait();

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

    fn check_sync_mapping(audio_latency: Option<u32>, first_seq: u16, first_rtp: u32) {
        let fake = Fake::start(Behavior {
            audio_latency,
            ..Behavior::default()
        });
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let renders = Renders::default();
        let settings = Settings {
            first_seq,
            first_rtp,
            ..settings()
        };
        let stream = Stream::start_with(&browser, fake.id(), Ok(settings), renders.renderer(None));
        assert!(wait_until(Duration::from_secs(4), || fake.syncs().len() >= 3));
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
        let receiver_latency = audio_latency.unwrap_or(0);
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
    fn sync_packets_place_each_packet_at_its_play_time() {
        check_sync_mapping(None, 1000, 5_000_000);
    }

    #[test]
    fn sync_packets_account_for_the_receivers_audio_latency() {
        check_sync_mapping(Some(11_025), 1000, 5_000_000);
    }

    #[test]
    fn sequence_numbers_and_rtp_times_wrap_without_a_seam() {
        check_sync_mapping(
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
    fn finishing_plays_out_answers_retransmissions_then_tears_down() {
        let fake = Fake::start(Behavior::default());
        let browser = fake.browser(SHAIRPORT_SYNC_RAOP);
        let renders = Renders::default();
        let stream =
            Stream::start_with(&browser, fake.id(), Ok(settings()), renders.renderer(None));
        assert!(wait_until(Duration::from_secs(2), || fake.audio().len() >= 40));
        let finishing = stream.finish();
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
            first_seq: 0u16.wrapping_sub(tail_from as u16 + 3),
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
        let finishing = stream.finish();
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
        stream.finish().wait();
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
    fn finishing_is_bounded_while_the_renderer_blocks_but_the_receiver_stays_taken() {
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
            let started = Instant::now();
            stream.finish().wait();
            let _ = waited.send(started.elapsed());
        });
        let elapsed = wait_returned.recv_timeout(bound + PROMPT);
        let taken = next.status() == Status::Connecting;
        drop(release);
        finishing.join().unwrap();
        let elapsed = elapsed.expect("finishing waited past its bound");
        assert!(
            elapsed >= bound && elapsed < bound + PROMPT,
            "waited {elapsed:?}"
        );
        assert!(taken, "the receiver was freed while the session still ran");
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
            teardown_at - dropped_at < LATENCY / 2,
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
        first.finish().wait();
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
        fake.advertise_airplay(&browser);
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
        fake.advertise_airplay(&browser);
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
            reason.starts_with("fake: it offers only AirPlay 2"),
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
            Ok(looking_up),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        stream.finish().wait();
        assert!(
            started.elapsed() < PROMPT,
            "lookup cancelled after {:?}",
            started.elapsed()
        );

        let fake = Fake::start(Behavior {
            rtsp: Rtsp::HangAtAnnounce,
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
        stream.finish().wait();
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
        waiting.finish().wait();
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
        first.finish().wait();
        thread::sleep(Duration::from_millis(200));
        assert!(
            fake.requests()
                .iter()
                .all(|(_, connection, _)| *connection == 0),
            "a cancelled stream connected"
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
    fn unsupported_or_unreachable_receivers_say_why() {
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

        let closed_port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let browser = Browser::offline();
        let key = InstanceKey {
            kind: Kind::Raop,
            name: INSTANCE.into(),
            interface: 1,
        };
        browser.shared().apply([
            Event::Resolved {
                key: key.clone(),
                host: "gone.local.".into(),
                port: closed_port,
                txt: txt(SHAIRPORT_SYNC_RAOP),
            },
            Event::AddressAdded {
                key,
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                scope: 0,
            },
        ]);
        let stream = Stream::start_with(
            &browser,
            ID.parse().unwrap(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(Duration::from_secs(2), || stream.status()
            != Status::Connecting));
        let Status::Unreachable(reason) = stream.status() else {
            panic!("{:?}", stream.status())
        };
        assert!(
            reason.starts_with(&format!(
                "fake: could not connect to 127.0.0.1:{closed_port}"
            )),
            "{reason}"
        );
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
