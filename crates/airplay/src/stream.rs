use std::any::Any;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::os::fd::{AsRawFd, RawFd};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::airplay2::{self, Failure};
use crate::backlog::Backlog;
use crate::browse;
use crate::clock::{self, Clock, PACKET_DURATION, Timeline};
use crate::packet::{self, AudioHeader, MAX_AUDIO_PACKET_LEN, MAX_RETRANSMISSION_LEN, Sync};
use crate::raop;
use crate::rtsp::{Connection, OpenError, ReadError, Request};
use crate::session::{Offer, Session};
use crate::turns::Turn;
use crate::{
    Browser, Counters, FRAMES_PER_PACKET, LATENCY, Protocol, Receiver, ReceiverId, Render, Status,
};

const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const SYNC_INTERVAL: Duration = Duration::from_secs(1);
const POLL_SLICE: Duration = Duration::from_millis(20);
/// Claude: a packet is sent only if it can reach the receiver this long before
/// it plays.
const MIN_LEAD: Duration = Duration::from_millis(5);
const WAKE_RETRY: Duration = Duration::from_millis(500);

/// Claude: a session playing to one receiver. It connects in the background:
/// it waits a bounded time for the browser to find the receiver, then for any
/// earlier stream through the same browser to that receiver to finish, then
/// picks its protocol from the receiver as browsing knows it by then and
/// connects, retrying for a bounded time while the receiver wakes from
/// standby, then calls `render` for each packet `LATENCY` before the packet's
/// play time.
/// Dropping it tears the session down at once; `finish` lets the receiver play
/// what it holds first.
pub struct Stream {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Stream {
    /// Claude: `render` runs on the stream's thread and must not block: a
    /// renderer that blocks holds up the stream and keeps the receiver taken.
    pub fn start<R>(browser: &Browser, receiver: ReceiverId, render: R) -> Self
    where
        R: FnMut(Render<'_>) + Send + 'static,
    {
        Self::start_with(browser, receiver, Settings::new(), render)
    }

    /// Claude: as `start`, speaking `protocol` and no other: a receiver that
    /// does not offer it, or refuses it, is unreachable.
    pub fn start_with_protocol<R>(
        browser: &Browser,
        receiver: ReceiverId,
        protocol: Protocol,
        render: R,
    ) -> Self
    where
        R: FnMut(Render<'_>) + Send + 'static,
    {
        let settings = Settings::new().map(|settings| Settings {
            protocol: Some(protocol),
            ..settings
        });
        Self::start_with(browser, receiver, settings, render)
    }

    pub(crate) fn start_with<R>(
        browser: &Browser,
        receiver: ReceiverId,
        settings: Result<Settings, String>,
        render: R,
    ) -> Self
    where
        R: FnMut(Render<'_>) + Send + 'static,
    {
        let shared = Arc::new(Shared {
            status: Mutex::new(Status::Connecting),
            command: AtomicU8::new(Command::Play as u8),
            clock: Clock::start(),
            finish_bound: settings
                .as_ref()
                .map_or(Duration::ZERO, |s| s.latency + s.teardown_timeout),
            ended: Mutex::new(false),
            ending: Condvar::new(),
        });
        let browser = browser.shared().clone();
        let turn = browser.turns.take(receiver.clone());
        let thread = settings.and_then(|settings| {
            let shared = shared.clone();
            thread::Builder::new()
                .name("airplay-stream".into())
                .spawn(move || {
                    let _ended = Ended(&shared);
                    run(&shared, &browser, &receiver, turn, settings, render);
                })
                .map_err(|e| format!("could not start the stream's thread ({e})"))
        });
        let thread = thread
            .inspect_err(|reason| shared.set_status(Status::Unreachable(reason.clone())))
            .ok();
        Stream { shared, thread }
    }

    pub fn status(&self) -> Status {
        self.shared
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Claude: stops rendering and returns at once. The session keeps answering
    /// the receiver until the end of the last packet it sent has played, then
    /// tears down, within a bounded time.
    pub fn finish(mut self) -> Finishing {
        self.shared
            .command
            .store(Command::Finish as u8, Ordering::Release);
        Finishing {
            deadline: Instant::now() + self.shared.finish_bound,
            shared: self.shared.clone(),
            thread: self.thread.take(),
        }
    }

    #[cfg(test)]
    pub(crate) fn clock(&self) -> Clock {
        self.shared.clock
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if self.thread.take().is_some() {
            self.shared
                .command
                .store(Command::Stop as u8, Ordering::Release);
        }
    }
}

/// Claude: a stream playing out its last packets. The receiver is still taken
/// until it is finished.
pub struct Finishing {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    deadline: Instant,
}

impl Finishing {
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Claude: blocks until the session has torn down, at most `LATENCY` plus
    /// the teardown timeout from `finish`. A session still running then, held
    /// up by its renderer, keeps the receiver taken until it ends.
    pub fn wait(mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let ended = self.shared.lock_ended();
        let timeout = self.deadline.saturating_duration_since(Instant::now());
        let (ended, _) = self
            .shared
            .ending
            .wait_timeout_while(ended, timeout, |ended| !*ended)
            .unwrap_or_else(PoisonError::into_inner);
        if *ended {
            drop(ended);
            let _ = thread.join();
        }
    }
}

struct Shared {
    status: Mutex<Status>,
    command: AtomicU8,
    clock: Clock,
    finish_bound: Duration,
    ended: Mutex<bool>,
    ending: Condvar,
}

/// Claude: tells a waiting `Finishing` that the session's thread is done, after
/// it has released everything, however it ends.
struct Ended<'a>(&'a Shared);

impl Drop for Ended<'_> {
    fn drop(&mut self) {
        *self.0.lock_ended() = true;
        self.0.ending.notify_all();
    }
}

impl Shared {
    fn command(&self) -> Command {
        match self.command.load(Ordering::Acquire) {
            0 => Command::Play,
            1 => Command::Finish,
            _ => Command::Stop,
        }
    }

    fn set_status(&self, status: Status) {
        *self.status.lock().unwrap_or_else(PoisonError::into_inner) = status;
    }

    fn lock_ended(&self) -> MutexGuard<'_, bool> {
        self.ended.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Play = 0,
    Finish = 1,
    Stop = 2,
}

#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) latency: Duration,
    pub(crate) lookup_window: Duration,
    pub(crate) first_seq: u16,
    pub(crate) first_rtp: u32,
    pub(crate) connect_timeout: Duration,
    pub(crate) wake_window: Duration,
    pub(crate) answer_timeout: Duration,
    pub(crate) teardown_timeout: Duration,
    pub(crate) timing_silence: Duration,
    pub(crate) send_failure_limit: Duration,
    /// Claude: the protocol the stream must speak, or `None` to speak the one
    /// `Receiver::protocol` picks.
    pub(crate) protocol: Option<Protocol>,
    #[cfg(test)]
    pub(crate) failing_sends: Arc<AtomicBool>,
}

impl Settings {
    pub(crate) fn new() -> Result<Settings, String> {
        let random = |e: getrandom::Error| format!("no randomness ({e})");
        Ok(Settings {
            latency: LATENCY,
            lookup_window: Duration::from_secs(3),
            first_seq: getrandom::u32().map_err(random)? as u16,
            first_rtp: getrandom::u32().map_err(random)?,
            connect_timeout: Duration::from_secs(2),
            wake_window: Duration::from_secs(15),
            answer_timeout: Duration::from_secs(4),
            teardown_timeout: Duration::from_secs(1),
            timing_silence: Duration::from_secs(15),
            send_failure_limit: Duration::from_secs(1),
            protocol: None,
            #[cfg(test)]
            failing_sends: Arc::default(),
        })
    }
}

fn run<R: FnMut(Render<'_>)>(
    shared: &Shared,
    browser: &browse::Shared,
    receiver: &ReceiverId,
    turn: Turn,
    settings: Settings,
    render: R,
) {
    let mut streamer = match connect(shared, browser, receiver, &turn, settings, render) {
        Ok(streamer) => streamer,
        Err(reason) => {
            if shared.command() == Command::Play {
                warn!(%receiver, %reason, "AirPlay receiver unreachable");
                shared.set_status(Status::Unreachable(reason));
            }
            return;
        }
    };
    info!(
        %receiver,
        protocol = %streamer.session.protocol(),
        timing = "NTP",
        address = %streamer.session.connection.peer_addr(),
        audio_latency = ?streamer.session.audio_latency,
        "AirPlay stream started"
    );
    shared.set_status(Status::Streaming(Counters::default()));
    let end = streamer.play();
    if let End::Lost(reason) = &end {
        warn!(%receiver, %reason, "AirPlay stream lost");
        shared.set_status(Status::Lost(reason.clone()));
    }
    streamer.teardown();
    info!(%receiver, ?end, counters = ?streamer.counters, late_packets = streamer.late_packets, "AirPlay stream ended");
    drop(streamer);
    drop(turn);
}

fn connect<'a, R>(
    shared: &'a Shared,
    browser: &browse::Shared,
    receiver: &ReceiverId,
    turn: &Turn,
    settings: Settings,
    render: R,
) -> Result<Streamer<'a, R>, String> {
    let abort = || shared.command() != Command::Play;
    let choose = |found: &Receiver| match settings.protocol {
        Some(protocol) => found.service(protocol).map(|_| protocol),
        None => found.protocol(),
    };
    let settled = |found: &Receiver| match choose(found) {
        Ok(Protocol::AirPlay1) => settings.protocol.is_some() || !found.awaits_airplay2(),
        chosen => chosen.is_ok(),
    };
    let found = browser
        .find(receiver, settings.lookup_window, settled, &abort)?
        .ok_or_else(|| {
            format!(
                "no AirPlay receiver {receiver} was found within {:?}",
                settings.lookup_window
            )
        })?;
    let protocol = choose(&found).map_err(|reason| format!("{}: {reason}", found.name))?;
    if !turn.wait(&abort) {
        return Err("stopped".into());
    }
    let (found, protocol) = match browser.receiver(receiver) {
        Some(now) => match choose(&now) {
            Ok(protocol) => (now, protocol),
            Err(_) => (found, protocol),
        },
        None => (found, protocol),
    };
    let name = found.name.clone();
    let connector = Connector {
        browser,
        receiver,
        settings: &settings,
        client: Client::new()?,
        clock: shared.clock,
        abort: &abort,
    };
    let link = match connector.attempt(&found, protocol) {
        Ok(link) => link,
        Err(failure) => {
            let reason = format!("{name}: {}", failure.reason());
            let Some(now) = fallback(&settings, protocol, &failure, browser, receiver) else {
                return Err(reason);
            };
            warn!(%receiver, %reason, "AirPlay 2 refused; falling back to AirPlay 1");
            connector
                .attempt(&now, Protocol::AirPlay1)
                .map_err(|failure| format!("{name}: {}", failure.reason()))?
        }
    };
    let Link {
        mut session,
        control,
        audio,
        timing,
    } = link;

    let latency_frames = clock::frames(settings.latency);
    let audio_latency = session.audio_latency.frames();
    if audio_latency >= latency_frames {
        session.teardown(settings.teardown_timeout);
        return Err(format!(
            "{name}: it delays audio by {audio_latency} frames, not less than the stream's \
             latency of {latency_frames} frames"
        ));
    }
    let backlog_packets =
        (settings.latency + Duration::from_secs(1)).as_nanos() / PACKET_DURATION.as_nanos() + 1;
    let now = Instant::now();
    Ok(Streamer {
        shared,
        timeline: Timeline {
            anchor: now + settings.latency,
            first_seq: settings.first_seq,
            first_rtp: settings.first_rtp,
        },
        latency_frames,
        audio_latency,
        ssrc: getrandom::u32().map_err(|e| e.to_string())?,
        keepalive: Keepalive {
            next_at: now + session.keepalive().every,
            outstanding: None,
        },
        settings,
        render,
        session,
        control,
        audio,
        timing,
        backlog: Backlog::new(backlog_packets as usize),
        left: Box::new([0.0; FRAMES_PER_PACKET]),
        right: Box::new([0.0; FRAMES_PER_PACKET]),
        packet: Box::new([0; MAX_AUDIO_PACKET_LEN]),
        retransmission: Box::new([0; MAX_RETRANSMISSION_LEN]),
        next: 0,
        next_sync: now,
        synced: false,
        failing_since: None,
        counters: Counters::default(),
        late_packets: 0,
    })
}

/// Claude: the receiver to try AirPlay 1 with after a handshake failed: only
/// when the stream picked AirPlay 2 itself, the receiver refused it rather
/// than failed, and the receiver offers AirPlay 1 as browsing knows it now.
fn fallback(
    settings: &Settings,
    protocol: Protocol,
    failure: &Failure,
    browser: &browse::Shared,
    receiver: &ReceiverId,
) -> Option<Receiver> {
    let refused = matches!(failure, Failure::Refused(_));
    if settings.protocol.is_some() || protocol != Protocol::AirPlay2 || !refused {
        return None;
    }
    browser
        .receiver(receiver)
        .filter(|now| now.service(Protocol::AirPlay1).is_ok())
}

/// Claude: how a stream names itself to the receiver, the same for each
/// protocol it tries.
struct Client {
    id: String,
    active_remote: String,
}

impl Client {
    fn new() -> Result<Client, String> {
        Ok(Client {
            id: format!("{:016X}", getrandom::u64().map_err(|e| e.to_string())?),
            active_remote: getrandom::u32().map_err(|e| e.to_string())?.to_string(),
        })
    }

    fn headers(&self, protocol: Protocol) -> Vec<(&'static str, String)> {
        let user_agent = match protocol {
            Protocol::AirPlay1 => USER_AGENT,
            Protocol::AirPlay2 => airplay2::USER_AGENT,
        };
        vec![
            ("User-Agent", user_agent.to_owned()),
            ("Client-Instance", self.id.clone()),
            ("DACP-ID", self.id.clone()),
            ("Active-Remote", self.active_remote.clone()),
        ]
    }
}

/// Claude: a session and the sockets its packets go through.
struct Link {
    session: Session,
    control: UdpSocket,
    audio: UdpSocket,
    timing: TimingResponder,
}

/// Claude: what each handshake of one stream shares.
struct Connector<'a> {
    browser: &'a browse::Shared,
    receiver: &'a ReceiverId,
    settings: &'a Settings,
    client: Client,
    clock: Clock,
    abort: &'a dyn Fn() -> bool,
}

impl Connector<'_> {
    /// Claude: one handshake speaking `protocol`, holding nothing once it
    /// fails.
    fn attempt(&self, found: &Receiver, protocol: Protocol) -> Result<Link, Failure> {
        let failed = |reason: String| Failure::Failed(reason);
        let service = found.service(protocol).map_err(failed)?;
        let connection = self
            .open(protocol, service.addresses.clone())
            .map_err(failed)?;
        let bind = || {
            let any = match connection.local_addr() {
                SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
                SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)),
            };
            UdpSocket::bind(any).map_err(|e| failed(format!("could not open a UDP socket ({e})")))
        };
        let (control, timing, audio) = (bind()?, bind()?, bind()?);
        control
            .set_nonblocking(true)
            .map_err(|e| failed(e.to_string()))?;
        let port = |socket: &UdpSocket| {
            socket
                .local_addr()
                .map(|a| a.port())
                .map_err(|e| failed(e.to_string()))
        };
        let offer = Offer {
            control_port: port(&control)?,
            timing_port: port(&timing)?,
            first_seq: self.settings.first_seq,
            first_rtp: self.settings.first_rtp,
        };
        let timing = TimingResponder::start(timing, self.clock).map_err(failed)?;
        let timeout = self.settings.answer_timeout;
        let session = match protocol {
            Protocol::AirPlay1 => {
                raop::negotiate(connection, &service.txt, &offer, timeout, self.abort)
                    .map_err(failed)?
            }
            Protocol::AirPlay2 => airplay2::negotiate(
                connection,
                &offer,
                timeout,
                self.settings.teardown_timeout,
                self.abort,
            )?,
        };
        Ok(Link {
            session,
            control,
            audio,
            timing,
        })
    }

    /// Claude: connects to the receiver, trying again while it fails as one
    /// waking from standby does, until `wake_window` ends: no attempt runs past
    /// it. Each try goes to the addresses browsing knows by then, since a
    /// waking receiver may come back on others.
    fn open(
        &self,
        protocol: Protocol,
        mut addresses: Vec<SocketAddr>,
    ) -> Result<Connection, String> {
        let (settings, abort, receiver) = (self.settings, self.abort, self.receiver);
        let common = self.client.headers(protocol);
        let deadline = Instant::now() + settings.wake_window;
        let mut tries = 0u32;
        loop {
            if abort() {
                return Err("stopped".into());
            }
            let tried_at = Instant::now();
            let opened = Connection::open(
                &addresses,
                settings.connect_timeout,
                deadline,
                common.clone(),
                abort,
            );
            let reason = match opened {
                Ok(connection) => return Ok(connection),
                Err(OpenError::Transient(reason)) if Instant::now() < deadline => reason,
                Err(error) => return Err(error.to_string()),
            };
            tries += 1;
            if tries == 1 {
                info!(%receiver, %reason, "AirPlay receiver not answering; trying again while it wakes");
            } else {
                debug!(%receiver, %reason, tries, "AirPlay receiver still not answering");
            }
            if !pause_until((tried_at + WAKE_RETRY).min(deadline), abort) {
                return Err("stopped".into());
            }
            if Instant::now() >= deadline {
                return Err(reason);
            }
            if let Some(found) = self.browser.receiver(receiver)
                && let Ok(service) = found.service(protocol)
            {
                addresses = service.addresses.clone();
            }
        }
    }
}

/// Claude: false when `abort` turned true first.
fn pause_until(until: Instant, abort: &dyn Fn() -> bool) -> bool {
    loop {
        if abort() {
            return false;
        }
        let now = Instant::now();
        if now >= until {
            return true;
        }
        thread::sleep((until - now).min(POLL_SLICE));
    }
}

/// Claude: a session from its RECORD on: it renders and sends each packet on
/// time and answers the receiver until it ends.
struct Streamer<'a, R> {
    shared: &'a Shared,
    settings: Settings,
    render: R,
    session: Session,
    control: UdpSocket,
    audio: UdpSocket,
    timing: TimingResponder,
    timeline: Timeline,
    latency_frames: u32,
    audio_latency: u32,
    ssrc: u32,
    backlog: Backlog,
    left: Box<[f32; FRAMES_PER_PACKET]>,
    right: Box<[f32; FRAMES_PER_PACKET]>,
    packet: Box<[u8; MAX_AUDIO_PACKET_LEN]>,
    retransmission: Box<[u8; MAX_RETRANSMISSION_LEN]>,
    next: u64,
    next_sync: Instant,
    synced: bool,
    failing_since: Option<Instant>,
    keepalive: Keepalive,
    counters: Counters,
    late_packets: u64,
}

struct Keepalive {
    next_at: Instant,
    outstanding: Option<(u32, Instant)>,
}

#[derive(Debug)]
enum End {
    Finished,
    Stopped,
    Lost(String),
}

impl<R: FnMut(Render<'_>)> Streamer<'_, R> {
    fn play(&mut self) -> End {
        loop {
            let step = match self.shared.command() {
                Command::Stop => return End::Stopped,
                Command::Finish => {
                    let Some(last) = self.backlog.latest() else {
                        return End::Finished;
                    };
                    let end = self.timeline.play_at(last + 1);
                    if Instant::now() >= end {
                        return End::Finished;
                    }
                    self.sync_if_due();
                    self.wait_until(end.min(self.next_sync))
                }
                Command::Play => {
                    let due = self.timeline.play_at(self.next) - self.settings.latency;
                    self.wait_until(due).and_then(|()| {
                        if self.shared.command() == Command::Play && Instant::now() >= due {
                            self.play_next()
                        } else {
                            Ok(())
                        }
                    })
                }
            };
            if let Err(reason) = step {
                return End::Lost(reason);
            }
        }
    }

    fn play_next(&mut self) -> Result<(), String> {
        let cutoff = Instant::now() + MIN_LEAD;
        let mut late = 0;
        while self.timeline.play_at(self.next) <= cutoff {
            self.next += 1;
            late += 1;
        }
        let packet = self.next;
        self.next += 1;
        let play_at = self.timeline.play_at(packet);
        self.left.fill(0.0);
        self.right.fill(0.0);
        let render = &mut self.render;
        let channels = [&mut self.left[..], &mut self.right[..]];
        panic::catch_unwind(AssertUnwindSafe(|| render(Render { channels, play_at })))
            .map_err(|payload| format!("the renderer panicked: {}", panic_message(&*payload)))?;
        if Instant::now() + MIN_LEAD < play_at {
            self.sync_if_due();
            self.send(packet)?;
        } else {
            late += 1;
        }
        if late > 0 {
            if self.late_packets == 0 {
                warn!(
                    late,
                    "AirPlay stream fell behind; dropped packets too late to play"
                );
            } else {
                debug!(late, "AirPlay stream dropped late packets");
            }
            self.late_packets += late;
        }
        Ok(())
    }

    /// Claude: sends a packet, keeping it for retransmission even when sending
    /// fails, and gives up once every send has failed for the limit.
    fn send(&mut self, packet: u64) -> Result<(), String> {
        let seq = self.timeline.seq(packet);
        let header = AudioHeader {
            seq,
            timestamp: self.timeline.rtp(packet as i64 * FRAMES_PER_PACKET as i64),
            ssrc: self.ssrc,
            marker: self.backlog.latest().is_none(),
        };
        let len = self
            .session
            .write_audio(&header, &self.left, &self.right, &mut self.packet);
        let sent = self.send_audio(len);
        self.backlog.push(packet, seq, &self.packet[..len]);
        match sent {
            Ok(sent) => {
                self.failing_since = None;
                self.counters.packets_sent += 1;
                self.counters.bytes_sent += sent as u64;
                self.shared.set_status(Status::Streaming(self.counters));
                Ok(())
            }
            Err(e) => {
                let now = Instant::now();
                let failing_for = now - *self.failing_since.get_or_insert(now);
                debug!(seq, error = %e, "could not send an audio packet");
                if failing_for >= self.settings.send_failure_limit {
                    return Err(format!("could not send audio for {failing_for:?} ({e})"));
                }
                Ok(())
            }
        }
    }

    fn send_audio(&self, len: usize) -> std::io::Result<usize> {
        #[cfg(test)]
        if self.settings.failing_sends.load(Ordering::Acquire) {
            return Err(std::io::Error::other("injected failure"));
        }
        self.audio.send_to(&self.packet[..len], self.session.audio)
    }

    fn sync_if_due(&mut self) {
        let now = Instant::now();
        if now < self.next_sync {
            return;
        }
        self.next_sync = now + SYNC_INTERVAL;
        let frame = self.timeline.frame_playing(now);
        let sync = Sync {
            first: !self.synced,
            playing: self.timeline.rtp(frame + i64::from(self.audio_latency)),
            at: self.shared.clock.ntp(self.timeline.frame_at(frame)),
            sending: self.timeline.rtp(frame + i64::from(self.latency_frames)),
        };
        self.synced = true;
        if let Err(e) = self
            .control
            .send_to(&packet::sync_packet(&sync), self.session.control)
        {
            debug!(error = %e, "could not send a sync packet");
        }
    }

    /// Claude: answers the receiver until `deadline`, or until the command
    /// changes, polling once even when the deadline has passed.
    fn wait_until(&mut self, deadline: Instant) -> Result<(), String> {
        let command = self.shared.command();
        loop {
            let timeout = deadline
                .saturating_duration_since(Instant::now())
                .min(POLL_SLICE);
            let [control, rtsp] = readable(
                [self.control.as_raw_fd(), self.session.connection.fd()],
                timeout,
            );
            if control {
                self.answer_control();
            }
            if rtsp {
                self.read_rtsp()?;
            }
            self.check_receiver()?;
            if Instant::now() >= deadline || self.shared.command() != command {
                return Ok(());
            }
        }
    }

    fn answer_control(&mut self) {
        let mut request = [0u8; 64];
        loop {
            let (len, from) = match self.control.recv_from(&mut request) {
                Ok(received) => received,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    debug!(error = %e, "control socket error");
                    break;
                }
            };
            let Some((first, count)) = packet::retransmit_request(&request[..len]) else {
                debug!(len, "ignoring a control packet");
                continue;
            };
            for seq in (0..count).map(|i| first.wrapping_add(i)) {
                let Some(original) = self.backlog.get(seq) else {
                    debug!(seq, "retransmit request for a packet no longer held");
                    continue;
                };
                let len = packet::write_retransmission(original, &mut self.retransmission);
                if let Ok(sent) = self.control.send_to(&self.retransmission[..len], from) {
                    self.counters.packets_retransmitted += 1;
                    self.counters.bytes_sent += sent as u64;
                }
            }
            self.shared.set_status(Status::Streaming(self.counters));
        }
    }

    fn read_rtsp(&mut self) -> Result<(), String> {
        let needs_ok = self.session.keepalive().needs_ok;
        let connection = &mut self.session.connection;
        match connection.read_ready() {
            Ok(()) | Err(ReadError::TimedOut) => {}
            Err(ReadError::Failed(reason)) => return Err(reason),
        }
        while let Some(response) = connection.next_response()? {
            if let Some((cseq, _)) = self.keepalive.outstanding
                && response.cseq() == Some(cseq)
            {
                if response.status != 200 && needs_ok {
                    return Err(format!(
                        "it answered a keep-alive with {} {}",
                        response.status, response.reason
                    ));
                }
                self.keepalive.outstanding = None;
            }
        }
        Ok(())
    }

    fn check_receiver(&mut self) -> Result<(), String> {
        let now = Instant::now();
        if let Some((_, sent)) = self.keepalive.outstanding {
            if now - sent > self.settings.answer_timeout {
                return Err("it stopped answering RTSP".into());
            }
        } else if now >= self.keepalive.next_at {
            let keepalive = self.session.keepalive();
            self.keepalive.next_at = now + keepalive.every;
            let request = Request {
                method: keepalive.method,
                uri: keepalive.uri,
                headers: vec![],
                body: None,
            };
            let cseq = self.session.connection.send(&request)?;
            self.keepalive.outstanding = Some((cseq, now));
        }
        if let Some(last) = self.timing.last_request()
            && now.saturating_duration_since(last) > self.settings.timing_silence
        {
            return Err("it stopped requesting timing".into());
        }
        Ok(())
    }

    fn teardown(&mut self) {
        self.session.teardown(self.settings.teardown_timeout);
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

fn readable<const N: usize>(fds: [RawFd; N], timeout: Duration) -> [bool; N] {
    let mut pollfds = fds.map(|fd| libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    });
    let ms = timeout.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
    let ready = unsafe { libc::poll(pollfds.as_mut_ptr(), N as libc::nfds_t, ms) };
    if ready <= 0 {
        return [false; N];
    }
    pollfds.map(|p| p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
}

/// Claude: answers the receiver's timing requests on a thread of its own, so
/// that the times it reports are not held up by rendering.
struct TimingResponder {
    stop: Arc<AtomicBool>,
    last_request: Arc<AtomicU64>,
    started: Instant,
    thread: Option<JoinHandle<()>>,
}

impl TimingResponder {
    fn start(socket: UdpSocket, clock: Clock) -> Result<TimingResponder, String> {
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let last_request = Arc::new(AtomicU64::new(0));
        let started = Instant::now();
        let thread = thread::Builder::new()
            .name("airplay-timing".into())
            .spawn({
                let (stop, last_request) = (stop.clone(), last_request.clone());
                move || {
                    let mut request = [0u8; 64];
                    while !stop.load(Ordering::Acquire) {
                        let (len, from) = match socket.recv_from(&mut request) {
                            Ok(received) => received,
                            Err(e)
                                if matches!(
                                    e.kind(),
                                    ErrorKind::WouldBlock | ErrorKind::TimedOut
                                ) =>
                            {
                                continue;
                            }
                            Err(e) => {
                                debug!(error = %e, "timing socket error");
                                thread::sleep(Duration::from_millis(50));
                                continue;
                            }
                        };
                        let received = clock.now();
                        let Some(origin) = packet::timing_request_sent_at(&request[..len]) else {
                            continue;
                        };
                        let reply = packet::timing_reply(origin, received, clock.now());
                        if let Err(e) = socket.send_to(&reply, from) {
                            debug!(error = %e, "could not answer a timing request");
                        }
                        let first = last_request
                            .swap(started.elapsed().as_nanos() as u64 + 1, Ordering::Release)
                            == 0;
                        if first {
                            debug!(%from, "first timing request answered");
                        }
                    }
                }
            })
            .map_err(|e| format!("could not start the timing thread ({e})"))?;
        Ok(TimingResponder {
            stop,
            last_request,
            started,
            thread: Some(thread),
        })
    }

    fn last_request(&self) -> Option<Instant> {
        match self.last_request.load(Ordering::Acquire) {
            0 => None,
            nanos => Some(self.started + Duration::from_nanos(nanos - 1)),
        }
    }
}

impl Drop for TimingResponder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
