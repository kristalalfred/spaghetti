use std::net::{SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use coreaudio_sys::{AudioGetCurrentHostTime, AudioGetHostClockFrequency};
use spaghetti_common::ipc_protocol::{
    Destination, FlowState, FlowStatus, PeerRef, PeerRole, Request, Response, SourceSpec,
    TurnServer, WebRtcPeer,
};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use super::{ActivityState, DaemonState, Shared, dispatch, lock_state};
use crate::activity::FpsTracker;
use crate::codec::tests::measure;
use crate::device_manager::DeviceManager;
use crate::playout::{HealthReport, Playout};
use crate::ring::{CHANNELS, Ring};
use crate::sinks::{Sinks, SourceTiming};
use crate::stun;

mod turn;

const TONE_HZ: f64 = 1000.0;
const LEFT: f32 = 0.5;
const RIGHT: f32 = 0.25;

/// Claude: a daemon's request handling over a router of its own, with a device
/// config in a temporary directory that is removed with it.
struct Daemon {
    shared: Shared,
    dir: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn peer(role: PeerRole, link: &str) -> WebRtcPeer {
    WebRtcPeer {
        link: link.into(),
        generation: 1,
        role,
        stun: Vec::new(),
        turn: Vec::new(),
    }
}

impl Daemon {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "spaghetti-webrtc-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let shared = Shared {
            state: Mutex::new(DaemonState::new()),
            devices: DeviceManager::at(&dir.join("devices.json"), Box::new(|_| false)),
            activity: ActivityState {
                reader: Mutex::new(None),
                fps: FpsTracker::new(),
            },
            cancel: CancellationToken::new(),
        };
        Self { shared, dir }
    }

    async fn request(&self, request: Request) -> Response {
        dispatch(request, &self.shared).await
    }

    async fn create_source(&self, peer: WebRtcPeer) -> u32 {
        let request = Request::CreateSource {
            source: SourceSpec::WebRtc(peer),
            tag: None,
        };
        match self.request(request).await {
            Response::SourceCreated { source_id } => source_id,
            other => panic!("create source: {other:?}"),
        }
    }

    async fn create_route(&self, source_id: u32, peer: WebRtcPeer) -> u32 {
        let request = Request::CreateRoute {
            source_id,
            destination: Destination::WebRtc(peer),
            volume: 1.0,
        };
        match self.request(request).await {
            Response::RouteCreated { route_id } => route_id,
            other => panic!("create route: {other:?}"),
        }
    }

    fn fixture(&self, sample_rate: f64) -> (u32, Arc<Sinks>) {
        lock_state(&self.shared.state)
            .router
            .create_fixture_source(sample_rate)
    }

    fn sink(&self, source_id: u32) -> (Arc<Ring>, SourceTiming) {
        lock_state(&self.shared.state)
            .router
            .attach_fixture_sink(source_id)
    }

    async fn listed(&self, peer: PeerRef) -> (FlowStatus, Option<String>) {
        match (
            peer,
            self.request(Request::ListSources).await,
            self.request(Request::ListRoutes).await,
        ) {
            (PeerRef::Source(id), Response::Sources(sources), _) => sources
                .into_iter()
                .find(|source| source.source_id == id)
                .map(|source| (source.status, source.local_description))
                .expect("the source is listed"),
            (PeerRef::Route(id), _, Response::Routes(routes)) => routes
                .into_iter()
                .find(|route| route.route_id == id)
                .map(|route| (route.status, route.local_description))
                .expect("the route is listed"),
            other => panic!("listing: {other:?}"),
        }
    }

    async fn status(&self, peer: PeerRef) -> FlowStatus {
        self.listed(peer).await.0
    }

    async fn state(&self, peer: PeerRef) -> FlowState {
        self.status(peer).await.state
    }

    /// Claude: the peer's local description, once it publishes one.
    async fn description(&self, peer: PeerRef) -> String {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(sdp) = self.listed(peer).await.1 {
                return sdp;
            }
            assert!(
                Instant::now() < deadline,
                "{peer:?} published no local description"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn set_remote(&self, peer: PeerRef, generation: u32, sdp: &str) -> Response {
        let request = Request::SetRemoteDescription {
            peer,
            generation,
            sdp: sdp.into(),
        };
        self.request(request).await
    }

    async fn apply(&self, peer: PeerRef, sdp: &str) {
        let response = self.set_remote(peer, 1, sdp).await;
        assert!(matches!(response, Response::Ok), "{peer:?}: {response:?}");
    }

    /// Claude: how long the peer took to reach `state`, failing after `within`.
    async fn wait_for(&self, peer: PeerRef, state: FlowState, within: Duration) -> Duration {
        let start = Instant::now();
        loop {
            let now = self.state(peer).await;
            if now == state {
                return start.elapsed();
            }
            assert!(
                start.elapsed() < within,
                "{peer:?} is {now:?}, not {state:?}, after {within:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Claude: the offerer's offer to the answerer and the answer back, as the
    /// controller would carry them, each passed through `carry`.
    async fn exchange(
        &self,
        offerer: PeerRef,
        answerer: PeerRef,
        carry: impl Fn(&str, bool) -> String,
    ) {
        let offer = self.description(offerer).await;
        self.apply(answerer, &carry(&offer, true)).await;
        let answer = self.description(answerer).await;
        self.apply(offerer, &carry(&answer, false)).await;
    }
}

fn direct(sdp: &str, _: bool) -> String {
    sdp.to_string()
}

/// Claude: feeds a source in real time with a 1 kHz tone, `LEFT` loud on the
/// left and `RIGHT` on the right, until dropped. While paused it pushes nothing
/// and resumes where it stopped.
struct Producer {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Producer {
    fn start(sinks: Arc<Sinks>, rate: f64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let (stop, paused) = (Arc::clone(&stop), Arc::clone(&paused));
            move || {
                let chunk = (rate / 100.0) as usize;
                let (mut start, mut base, mut sent) = (Instant::now(), 0usize, 0usize);
                while !stop.load(Ordering::Relaxed) {
                    if paused.load(Ordering::Relaxed) {
                        (start, base) = (Instant::now(), sent);
                    } else {
                        let due = base + (start.elapsed().as_secs_f64() * rate) as usize;
                        while sent + chunk <= due {
                            let at = sent;
                            let now = unsafe { AudioGetCurrentHostTime() };
                            unsafe {
                                sinks.for_each(|ring| {
                                    ring.push_with(chunk, now, |f, ch| {
                                        let phase =
                                            2.0 * std::f64::consts::PI * TONE_HZ * (at + f) as f64
                                                / rate;
                                        [LEFT, RIGHT][ch] * phase.sin() as f32
                                    });
                                });
                            }
                            sent += chunk;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        });
        Self {
            stop,
            paused,
            thread: Some(thread),
        }
    }

    fn pause(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Claude: what reaches `ring` over the next `seconds`, interleaved.
async fn collect(ring: &Arc<Ring>, seconds: f64) -> Vec<f32> {
    let ring = Arc::clone(ring);
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        let mut buffer = vec![0.0; 4096 * CHANNELS];
        ring.discard(ring.available());
        let start = Instant::now();
        while start.elapsed().as_secs_f64() < seconds {
            let frames = ring.pop_into(&mut buffer);
            out.extend_from_slice(&buffer[..frames * CHANNELS]);
            std::thread::sleep(Duration::from_millis(10));
        }
        out
    })
    .await
    .unwrap()
}

/// Claude: the steady second half of `pcm` is the tone on both channels.
fn assert_tone(pcm: &[f32], label: &str) {
    assert!(
        pcm.len() / CHANNELS > 24000,
        "{label}: {} frames",
        pcm.len() / CHANNELS
    );
    let steady = &pcm[pcm.len() / 2..];
    for (offset, amplitude) in [(0, LEFT), (1, RIGHT)] {
        let (hz, rms) = measure(&steady[offset..], CHANNELS, 48000.0);
        assert!(
            (hz - TONE_HZ).abs() < 5.0,
            "{label}, channel {offset}: {hz} Hz"
        );
        let expected = amplitude / 2f32.sqrt();
        assert!(
            (rms - expected).abs() < 0.03,
            "{label}, channel {offset}: rms {rms}, expected {expected}"
        );
    }
}

/// Claude: renders `ring` for `seconds` the way a 48 kHz device route would, in
/// real time, and returns the left channel, the playout's health and the largest
/// correction its controller made, in ppm.
async fn play(
    ring: &Arc<Ring>,
    timing: SourceTiming,
    seconds: f64,
) -> (Vec<f32>, HealthReport, f32) {
    let ring = Arc::clone(ring);
    tokio::task::spawn_blocking(move || {
        let ticks_per_second = unsafe { AudioGetHostClockFrequency() };
        let mut playout = Playout::new(&timing, 48000.0, ticks_per_second).unwrap();
        let health = playout.health();
        let frames = 512;
        let period = Duration::from_secs_f64(frames as f64 / 48000.0);
        let (mut left, mut right) = (vec![0.0; frames], vec![0.0; frames]);
        let mut out = Vec::new();
        let start = Instant::now();
        let mut renders = 0u32;
        let mut worst_correction_ppm = 0.0f32;
        while start.elapsed().as_secs_f64() < seconds {
            let due = start + period * renders;
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            playout.render(&ring, &mut [&mut left, &mut right], unsafe {
                AudioGetCurrentHostTime()
            });
            out.extend_from_slice(&left);
            renders += 1;
            worst_correction_ppm = worst_correction_ppm.max(health.report().correction_ppm.abs());
        }
        (out, health.report(), worst_correction_ppm)
    })
    .await
    .unwrap()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Impairment {
    None,
    /// Claude: of the RTP packets toward the answerer, drops every 50th, holds
    /// another in 50 back 30 ms so it lands behind its successor, and holds
    /// every 17th 12 ms.
    LossAndReorder,
    /// Claude: nothing gets through either way.
    Blackhole,
}

/// Claude: a UDP relay between an offerer and an answerer that each believe
/// they talk to the other directly. It records the RTP sequence numbers and
/// timestamps going to the answerer; SRTP leaves the RTP header in the clear.
struct Shim {
    /// Claude: where the answerer is told the offerer is.
    as_offerer: SocketAddr,
    /// Claude: where the offerer is told the answerer is.
    as_answerer: SocketAddr,
    answerer: Arc<std::sync::OnceLock<SocketAddr>>,
    rtp: Arc<Mutex<Vec<(u16, u32)>>>,
    dropped: Arc<AtomicU64>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Shim {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn is_rtp(datagram: &[u8]) -> bool {
    datagram.len() >= 12 && datagram[0] >> 6 == 2 && !(192..=223).contains(&datagram[1])
}

impl Shim {
    /// Claude: carries an offer and its answer between the daemon's two peers
    /// through a new shim, which then relays all their traffic.
    async fn exchange(
        daemon: &Daemon,
        offerer: PeerRef,
        answerer: PeerRef,
        impairment: Impairment,
    ) -> Self {
        let offer = daemon.description(offerer).await;
        let shim = Self::from_offerer(host_candidate(&offer), impairment).await;
        daemon.apply(answerer, &shim.carry(&offer, true)).await;
        let answer = daemon.description(answerer).await;
        shim.answerer.set(host_candidate(&answer)).unwrap();
        daemon.apply(offerer, &shim.carry(&answer, false)).await;
        shim
    }

    async fn from_offerer(offerer: SocketAddr, impairment: Impairment) -> Self {
        let facing_offerer = Arc::new(UdpSocket::bind((offerer.ip(), 0)).await.unwrap());
        let facing_answerer = Arc::new(UdpSocket::bind((offerer.ip(), 0)).await.unwrap());
        let answerer = Arc::new(std::sync::OnceLock::new());
        let rtp = Arc::new(Mutex::new(Vec::new()));
        let dropped = Arc::new(AtomicU64::new(0));
        let forward = tokio::spawn({
            let (from, to) = (Arc::clone(&facing_offerer), Arc::clone(&facing_answerer));
            let (answerer, rtp, dropped) = (
                Arc::clone(&answerer),
                Arc::clone(&rtp),
                Arc::clone(&dropped),
            );
            async move {
                let mut buffer = vec![0u8; 2048];
                let mut packets = 0u64;
                loop {
                    let Ok((len, _)) = from.recv_from(&mut buffer).await else {
                        continue;
                    };
                    let datagram = buffer[..len].to_vec();
                    let Some(&answerer) = answerer.get() else {
                        continue;
                    };
                    if impairment == Impairment::Blackhole {
                        continue;
                    }
                    let mut delay = Duration::ZERO;
                    if is_rtp(&datagram) {
                        let seq = u16::from_be_bytes([datagram[2], datagram[3]]);
                        let ts = u32::from_be_bytes([
                            datagram[4],
                            datagram[5],
                            datagram[6],
                            datagram[7],
                        ]);
                        rtp.lock().unwrap().push((seq, ts));
                        let n = packets;
                        packets += 1;
                        if impairment == Impairment::LossAndReorder {
                            if n % 50 == 25 {
                                dropped.fetch_add(1, Ordering::Relaxed);
                                continue;
                            } else if n % 50 == 10 {
                                delay = Duration::from_millis(30);
                            } else if n % 17 == 3 {
                                delay = Duration::from_millis(12);
                            }
                        }
                    }
                    if delay.is_zero() {
                        let _ = to.send_to(&datagram, answerer).await;
                    } else {
                        let to = Arc::clone(&to);
                        tokio::spawn(async move {
                            tokio::time::sleep(delay).await;
                            let _ = to.send_to(&datagram, answerer).await;
                        });
                    }
                }
            }
        });
        let backward = tokio::spawn({
            let (from, to) = (Arc::clone(&facing_answerer), Arc::clone(&facing_offerer));
            async move {
                let mut buffer = vec![0u8; 2048];
                loop {
                    let Ok((len, _)) = from.recv_from(&mut buffer).await else {
                        continue;
                    };
                    if impairment != Impairment::Blackhole {
                        let _ = to.send_to(&buffer[..len], offerer).await;
                    }
                }
            }
        });
        Self {
            as_answerer: facing_offerer.local_addr().unwrap(),
            as_offerer: facing_answerer.local_addr().unwrap(),
            answerer,
            rtp,
            dropped,
            tasks: vec![forward, backward],
        }
    }

    /// Claude: rewrites a description so its sender's host candidate is this
    /// shim, and drops the other candidates.
    fn carry(&self, sdp: &str, from_offerer: bool) -> String {
        let address = if from_offerer {
            self.as_offerer
        } else {
            self.as_answerer
        };
        sdp.split("\r\n")
            .filter_map(|line| {
                if !line.starts_with("a=candidate:") {
                    return Some(line.to_string());
                }
                let mut fields: Vec<String> = line.split(' ').map(String::from).collect();
                if fields.get(7).map(String::as_str) != Some("host") {
                    return None;
                }
                fields[4] = address.ip().to_string();
                fields[5] = address.port().to_string();
                Some(fields.join(" "))
            })
            .collect::<Vec<_>>()
            .join("\r\n")
    }
}

fn host_candidate(sdp: &str) -> SocketAddr {
    sdp.split("\r\n")
        .filter(|line| line.starts_with("a=candidate:"))
        .find_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            (fields.get(7) == Some(&"host"))
                .then(|| SocketAddr::new(fields[4].parse().unwrap(), fields[5].parse().unwrap()))
        })
        .expect("a host candidate")
}

/// Claude: a STUN server that answers each request at once with `mapped`, and
/// answers it once more when `late` is notified.
async fn fake_stun(
    mapped: SocketAddrV4,
) -> (
    SocketAddr,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<()>,
) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let late = Arc::new(tokio::sync::Notify::new());
    let task = tokio::spawn({
        let late = Arc::clone(&late);
        async move {
            let mut buffer = vec![0u8; 2048];
            let (len, from) = socket.recv_from(&mut buffer).await.unwrap();
            let transaction = stun::transaction(&buffer[..len]).expect("a STUN request");
            let response = stun::tests::binding_response(transaction, mapped);
            socket.send_to(&response, from).await.unwrap();
            late.notified().await;
            socket.send_to(&response, from).await.unwrap();
        }
    });
    (address, late, task)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_offered_route_and_an_answering_source_carry_a_stereo_tone() {
    let daemon = Daemon::new();
    let (fixture, sinks) = daemon.fixture(48000.0);
    let _producer = Producer::start(sinks, 48000.0);
    let source = PeerRef::Source(daemon.create_source(peer(PeerRole::Answer, "tone")).await);
    let route = PeerRef::Route(
        daemon
            .create_route(fixture, peer(PeerRole::Offer, "tone"))
            .await,
    );
    let PeerRef::Source(source_id) = source else {
        unreachable!()
    };
    let (ring, _) = daemon.sink(source_id);

    let offer = daemon.description(route).await;
    assert_eq!(
        daemon.listed(source).await.1,
        None,
        "an answerer describes itself only after the offer"
    );
    assert!(offer.contains("a=sendonly"), "{offer}");
    daemon.apply(source, &offer).await;
    let answer = daemon.description(source).await;
    assert!(answer.contains("a=recvonly"), "{answer}");
    for sdp in [&offer, &answer] {
        let fmtp = sdp
            .lines()
            .find(|line| line.starts_with("a=fmtp:"))
            .expect("an fmtp line");
        assert!(
            fmtp.contains("stereo=1") && fmtp.contains("sprop-stereo=1"),
            "{fmtp}"
        );
    }
    daemon.apply(route, &answer).await;

    daemon
        .wait_for(source, FlowState::Flowing, Duration::from_secs(5))
        .await;
    daemon
        .wait_for(route, FlowState::Flowing, Duration::from_secs(5))
        .await;
    let pcm = collect(&ring, 2.0).await;
    assert_tone(&pcm, "received");

    let (sent, received) = (daemon.status(route).await, daemon.status(source).await);
    assert!(
        sent.bytes > 0 && received.bytes > 0,
        "{sent:?} {received:?}"
    );
    assert_eq!(received.packets_lost, 0);
    assert_eq!(received.peer, Some(host_candidate(&offer).to_string()));
    assert_eq!(sent.peer, Some(host_candidate(&answer).to_string()));
    assert!(!sent.relayed && !received.relayed);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_44_1_khz_source_advances_rtp_time_by_960_per_packet() {
    let daemon = Daemon::new();
    let (fixture, sinks) = daemon.fixture(44100.0);
    let _producer = Producer::start(sinks, 44100.0);
    let source = PeerRef::Source(daemon.create_source(peer(PeerRole::Answer, "cd")).await);
    let route = PeerRef::Route(
        daemon
            .create_route(fixture, peer(PeerRole::Offer, "cd"))
            .await,
    );
    let PeerRef::Source(source_id) = source else {
        unreachable!()
    };
    let (ring, _) = daemon.sink(source_id);

    let shim = Shim::exchange(&daemon, route, source, Impairment::None).await;
    daemon
        .wait_for(source, FlowState::Flowing, Duration::from_secs(5))
        .await;

    let pcm = collect(&ring, 3.0).await;
    assert_tone(&pcm, "received from 44.1 kHz");
    let rtp = shim.rtp.lock().unwrap().clone();
    assert!(rtp.len() >= 140, "{} RTP packets", rtp.len());
    let (first_seq, first_ts) = rtp[0];
    for (index, &(seq, ts)) in rtp.iter().enumerate() {
        assert_eq!(
            seq.wrapping_sub(first_seq) as usize,
            index,
            "packets reached the answerer out of order"
        );
        assert_eq!(
            ts.wrapping_sub(first_ts),
            960 * index as u32,
            "packet {index}"
        );
    }
    assert_eq!(daemon.status(source).await.packets_lost, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_receiver_stalls_when_packets_stop_and_flows_when_they_resume() {
    let daemon = Daemon::new();
    let (fixture, sinks) = daemon.fixture(48000.0);
    let producer = Producer::start(sinks, 48000.0);
    let source = PeerRef::Source(daemon.create_source(peer(PeerRole::Answer, "stall")).await);
    let route = PeerRef::Route(
        daemon
            .create_route(fixture, peer(PeerRole::Offer, "stall"))
            .await,
    );
    daemon.exchange(route, source, direct).await;
    daemon
        .wait_for(source, FlowState::Flowing, Duration::from_secs(5))
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    producer.pause(true);
    let stalled_after = daemon
        .wait_for(source, FlowState::Stalled, Duration::from_secs(3))
        .await;
    assert!(
        (Duration::from_millis(1200)..Duration::from_millis(2200)).contains(&stalled_after),
        "stalled {stalled_after:?} after the packets stopped"
    );
    producer.pause(false);
    let resumed_after = daemon
        .wait_for(source, FlowState::Flowing, Duration::from_secs(1))
        .await;
    assert!(
        resumed_after < Duration::from_millis(500),
        "flowing {resumed_after:?} after the packets resumed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stun_answer_in_time_becomes_a_candidate_and_a_late_one_changes_nothing() {
    let daemon = Daemon::new();
    let (fixture, sinks) = daemon.fixture(48000.0);
    let _producer = Producer::start(sinks, 48000.0);
    let mapped: SocketAddrV4 = "203.0.113.7:40000".parse().unwrap();
    let (stun_server, late, _stun) = fake_stun(mapped).await;
    let source = PeerRef::Source(daemon.create_source(peer(PeerRole::Answer, "stun")).await);
    let PeerRef::Source(source_id) = source else {
        unreachable!()
    };
    let (ring, _) = daemon.sink(source_id);
    let created = Instant::now();
    let offering = WebRtcPeer {
        stun: vec![stun_server.to_string()],
        ..peer(PeerRole::Offer, "stun")
    };
    let route = PeerRef::Route(daemon.create_route(fixture, offering).await);

    let offer = daemon.description(route).await;
    assert!(
        created.elapsed() < Duration::from_millis(900),
        "the answered STUN round took {:?}",
        created.elapsed()
    );
    assert!(offer.contains("203.0.113.7 40000 typ srflx"), "{offer}");
    daemon.exchange(route, source, direct).await;
    daemon
        .wait_for(source, FlowState::Flowing, Duration::from_secs(5))
        .await;
    daemon
        .wait_for(route, FlowState::Flowing, Duration::from_secs(5))
        .await;

    late.notify_one();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let pcm = collect(&ring, 2.0).await;
    assert_tone(&pcm, "after a late STUN answer");
    assert_eq!(daemon.state(route).await, FlowState::Flowing);
    assert_eq!(daemon.state(source).await, FlowState::Flowing);
}

#[tokio::test(flavor = "multi_thread")]
async fn garbage_on_the_media_sockets_does_not_end_the_link() {
    let daemon = Daemon::new();
    let (fixture, sinks) = daemon.fixture(48000.0);
    let _producer = Producer::start(sinks, 48000.0);
    let source = PeerRef::Source(
        daemon
            .create_source(peer(PeerRole::Answer, "garbage"))
            .await,
    );
    let route = PeerRef::Route(
        daemon
            .create_route(fixture, peer(PeerRole::Offer, "garbage"))
            .await,
    );
    let PeerRef::Source(source_id) = source else {
        unreachable!()
    };
    let (ring, _) = daemon.sink(source_id);
    daemon.exchange(route, source, direct).await;
    daemon
        .wait_for(source, FlowState::Flowing, Duration::from_secs(5))
        .await;

    let offer = daemon.description(route).await;
    let answer = daemon.description(source).await;
    let sender = UdpSocket::bind((host_candidate(&offer).ip(), 0))
        .await
        .unwrap();
    let mut stun_shaped = stun::binding_request(stun::transaction_id()).to_vec();
    stun_shaped[0] = 0x01;
    let datagrams: [Vec<u8>; 5] = [
        b"not a datagram anyone expects".to_vec(),
        (0..200u8).map(|i| i.wrapping_mul(37)).collect(),
        stun_shaped,
        [0x80, 111, 0, 9, 0, 0, 0, 1, 1, 2, 3, 4]
            .iter()
            .copied()
            .chain([0xaa; 100])
            .collect(),
        [22, 254, 253, 0, 0]
            .iter()
            .copied()
            .chain([0x55; 60])
            .collect(),
    ];
    for target in [host_candidate(&offer), host_candidate(&answer)] {
        for datagram in &datagrams {
            sender.send_to(datagram, target).await.unwrap();
        }
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    let pcm = collect(&ring, 2.0).await;
    assert_tone(&pcm, "after garbage");
    assert_eq!(daemon.state(route).await, FlowState::Flowing);
    assert_eq!(daemon.state(source).await, FlowState::Flowing);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_offer_describes_itself_after_the_stun_deadline_and_stays_connecting() {
    let daemon = Daemon::new();
    let (fixture, _sinks) = daemon.fixture(48000.0);
    let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let created = Instant::now();
    let offering = WebRtcPeer {
        stun: vec![silent.local_addr().unwrap().to_string()],
        ..peer(PeerRole::Offer, "unanswered")
    };
    let route = PeerRef::Route(daemon.create_route(fixture, offering).await);

    let not_ready = daemon.set_remote(route, 1, "v=0\r\n").await;
    assert!(
        matches!(&not_ready, Response::Error { message } if message.contains("not ready")),
        "{not_ready:?}"
    );
    let latest = Duration::from_secs(4);
    let offer = loop {
        if let Some(sdp) = daemon.listed(route).await.1 {
            break sdp;
        }
        assert!(
            created.elapsed() < latest,
            "not described after {:?}",
            created.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let described_after = created.elapsed();
    assert!(
        (Duration::from_millis(1950)..latest).contains(&described_after),
        "described after {described_after:?}"
    );
    assert!(
        offer.contains("typ host") && !offer.contains("typ srflx"),
        "{offer}"
    );
    while created.elapsed() < Duration::from_secs(12) {
        assert_eq!(
            daemon.state(route).await,
            FlowState::Connecting,
            "after {:?}",
            created.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_unreachable_deadline_waits_for_the_answer_however_late_it_comes() {
    let daemon = Daemon::new();
    let (fixture, sinks) = daemon.fixture(48000.0);
    let _producer = Producer::start(sinks, 48000.0);
    let source = PeerRef::Source(
        daemon
            .create_source(peer(PeerRole::Answer, "patient"))
            .await,
    );
    let route = PeerRef::Route(
        daemon
            .create_route(fixture, peer(PeerRole::Offer, "patient"))
            .await,
    );
    let created = Instant::now();

    let offer = daemon.description(route).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    daemon.apply(source, &offer).await;
    let answer = daemon.description(source).await;
    while created.elapsed() < Duration::from_millis(11500) {
        for peer in [route, source] {
            assert_eq!(
                daemon.state(peer).await,
                FlowState::Connecting,
                "{peer:?} after {:?}",
                created.elapsed()
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    daemon.apply(route, &answer).await;
    daemon
        .wait_for(source, FlowState::Flowing, Duration::from_secs(5))
        .await;
    daemon
        .wait_for(route, FlowState::Flowing, Duration::from_secs(5))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_applied_answer_whose_peer_is_silent_makes_the_offerer_unreachable_10_s_later() {
    let daemon = Daemon::new();
    let (fixture, _sinks) = daemon.fixture(48000.0);
    let source = PeerRef::Source(daemon.create_source(peer(PeerRole::Answer, "silent")).await);
    let route = PeerRef::Route(
        daemon
            .create_route(fixture, peer(PeerRole::Offer, "silent"))
            .await,
    );
    let created = Instant::now();
    let offer = daemon.description(route).await;
    let shim = Shim::from_offerer(host_candidate(&offer), Impairment::Blackhole).await;
    daemon.apply(source, &shim.carry(&offer, true)).await;
    let answer = shim.carry(&daemon.description(source).await, false);
    tokio::time::sleep(Duration::from_secs(3).saturating_sub(created.elapsed())).await;

    let applied = Instant::now();
    daemon.apply(route, &answer).await;
    while applied.elapsed() < Duration::from_millis(9500) {
        assert_eq!(
            daemon.state(route).await,
            FlowState::Connecting,
            "{:?} after the answer",
            applied.elapsed()
        );
        assert_eq!(daemon.state(source).await, FlowState::Connecting);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    daemon
        .wait_for(route, FlowState::Unreachable, Duration::from_secs(2))
        .await;
    let unreachable_after = applied.elapsed();
    assert!(
        unreachable_after < Duration::from_millis(11000),
        "unreachable {unreachable_after:?} after the answer"
    );
    assert_eq!(daemon.state(source).await, FlowState::Connecting);

    let (_, description) = daemon.listed(route).await;
    assert_eq!(description, Some(offer));
    let refused = daemon.set_remote(route, 1, &answer).await;
    assert!(
        matches!(&refused, Response::Error { message } if message.contains("unreachable")),
        "{refused:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_one_side_makes_the_other_lost() {
    let daemon = Daemon::new();
    let (fixture, sinks) = daemon.fixture(48000.0);
    let _producer = Producer::start(sinks, 48000.0);
    let source_id = daemon
        .create_source(peer(PeerRole::Answer, "closing"))
        .await;
    let (source, route) = (
        PeerRef::Source(source_id),
        PeerRef::Route(
            daemon
                .create_route(fixture, peer(PeerRole::Offer, "closing"))
                .await,
        ),
    );
    daemon.exchange(route, source, direct).await;
    daemon
        .wait_for(route, FlowState::Flowing, Duration::from_secs(5))
        .await;
    let answer = daemon.description(source).await;
    let offer = daemon.description(route).await;

    let destroyed = daemon.request(Request::DestroySource { source_id }).await;
    assert!(matches!(destroyed, Response::Ok), "{destroyed:?}");
    let lost_after = daemon
        .wait_for(route, FlowState::Lost, Duration::from_secs(1))
        .await;
    assert!(
        lost_after < Duration::from_millis(500),
        "lost {lost_after:?} after the close"
    );

    let (_, description) = daemon.listed(route).await;
    assert_eq!(description, Some(offer));
    let refused = daemon.set_remote(route, 1, &answer).await;
    assert!(
        matches!(&refused, Response::Error { message } if message.contains("lost")),
        "{refused:?}"
    );

    tokio::time::sleep(Duration::from_secs(1)).await;
    let PeerRef::Route(route_id) = route else {
        unreachable!()
    };
    let ring = lock_state(&daemon.shared.state).router.route_ring(route_id);
    assert_eq!(
        ring.overflows(),
        0,
        "the lost route stopped draining its ring"
    );
    assert_eq!(daemon.state(route).await, FlowState::Lost);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_description_applies_once_per_generation() {
    let daemon = Daemon::new();
    let (fixture, _sinks) = daemon.fixture(48000.0);
    let source = PeerRef::Source(daemon.create_source(peer(PeerRole::Answer, "once")).await);
    let route = PeerRef::Route(
        daemon
            .create_route(fixture, peer(PeerRole::Offer, "once"))
            .await,
    );
    let other_route = PeerRef::Route(
        daemon
            .create_route(fixture, peer(PeerRole::Offer, "other"))
            .await,
    );
    let receiving_offerer =
        PeerRef::Source(daemon.create_source(peer(PeerRole::Offer, "inbound")).await);
    let offer = daemon.description(route).await;
    let other_offer = daemon.description(other_route).await;

    let error = |response: Response| match response {
        Response::Error { message } => message,
        other => panic!("expected an error, got {other:?}"),
    };
    let wrong = error(daemon.set_remote(source, 2, &offer).await);
    assert!(wrong.contains('2') && wrong.contains('1'), "{wrong}");
    let unparsable = error(
        daemon
            .set_remote(source, 1, "not a session description")
            .await,
    );
    assert!(unparsable.contains("parse"), "{unparsable}");
    let inbound_offer = daemon.description(receiving_offerer).await;
    let wrong_way = error(daemon.set_remote(source, 1, &inbound_offer).await);
    assert!(wrong_way.contains("does not send"), "{wrong_way}");
    assert_eq!(
        daemon.listed(source).await.1,
        None,
        "a refused offer is not applied"
    );

    daemon.apply(source, &offer).await;
    let answer = daemon.description(source).await;
    daemon.apply(source, &offer).await;
    assert_eq!(daemon.listed(source).await.1, Some(answer.clone()));
    let different = error(daemon.set_remote(source, 1, &other_offer).await);
    assert!(different.contains("different"), "{different}");

    daemon.apply(route, &answer).await;
    daemon.apply(route, &answer).await;
    let edited = answer.replacen("a=recvonly", "a=recvonly\r\na=x-edited:1", 1);
    let different = error(daemon.set_remote(route, 1, &edited).await);
    assert!(different.contains("different"), "{different}");
    daemon
        .wait_for(route, FlowState::Connected, Duration::from_secs(5))
        .await;

    let missing = error(daemon.set_remote(PeerRef::Route(999), 1, &answer).await);
    assert!(missing.contains("not found"), "{missing}");
    let not_webrtc = error(
        daemon
            .set_remote(PeerRef::Source(fixture), 1, &answer)
            .await,
    );
    assert!(not_webrtc.contains("not a WebRTC peer"), "{not_webrtc}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_takes_stun_and_turn_servers_only_as_host_and_port() {
    let daemon = Daemon::new();
    let turn = |address: &str| TurnServer {
        address: address.into(),
        username: "user".into(),
        credential: "secret".into(),
    };
    let refused = [
        (
            vec!["stun.example.net".to_string()],
            Vec::new(),
            "stun.example.net",
        ),
        (vec![":3478".to_string()], Vec::new(), ":3478"),
        (
            Vec::new(),
            vec![turn("turn.example.net:turn")],
            "turn.example.net:turn",
        ),
    ];
    for (stun, turn, named) in refused {
        let spec = WebRtcPeer {
            stun,
            turn,
            ..peer(PeerRole::Answer, "servers")
        };
        let response = daemon
            .request(Request::CreateSource {
                source: SourceSpec::WebRtc(spec),
                tag: None,
            })
            .await;
        assert!(
            matches!(&response, Response::Error { message } if message.contains(named) && message.contains("host:port")),
            "{response:?}"
        );
    }
    let accepted = WebRtcPeer {
        stun: vec!["127.0.0.1:3478".into()],
        turn: vec![turn("turn.example.net:3478")],
        ..peer(PeerRole::Answer, "servers")
    };
    daemon.create_source(accepted).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concealment_keeps_a_lossy_reordered_stream_continuous() {
    let daemon = Daemon::new();
    let (fixture, sinks) = daemon.fixture(48000.0);
    let _producer = Producer::start(sinks, 48000.0);
    let source = PeerRef::Source(daemon.create_source(peer(PeerRole::Answer, "lossy")).await);
    let route = PeerRef::Route(
        daemon
            .create_route(fixture, peer(PeerRole::Offer, "lossy"))
            .await,
    );
    let PeerRef::Source(source_id) = source else {
        unreachable!()
    };
    let (ring, timing) = daemon.sink(source_id);

    let shim = Shim::exchange(&daemon, route, source, Impairment::LossAndReorder).await;
    daemon
        .wait_for(source, FlowState::Flowing, Duration::from_secs(5))
        .await;

    let (rendered, health, worst_correction_ppm) = play(&ring, timing, 20.0).await;
    let dropped = shim.dropped.load(Ordering::Relaxed);
    let concealed = daemon.status(source).await.packets_lost;
    assert!(dropped >= 18, "the shim dropped {dropped} packets");
    assert_eq!(
        [health.underruns, health.reprimes],
        [0, 0],
        "the playout underran or re-primed"
    );
    assert!(
        worst_correction_ppm < 3000.0,
        "single losses moved the target: {worst_correction_ppm} ppm"
    );
    assert!(
        concealed.abs_diff(dropped) <= 1,
        "concealed {concealed} of {dropped} dropped packets"
    );
    let steady = &rendered[rendered.len() / 2..];
    let (hz, rms) = measure(steady, 1, 48000.0);
    assert!((hz - TONE_HZ).abs() < 5.0, "{hz} Hz");
    assert!((rms - LEFT / 2f32.sqrt()).abs() < 0.03, "rms {rms}");
}
