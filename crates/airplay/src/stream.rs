mod realtime;

use std::any::Any;
use std::net::SocketAddr;
use std::os::fd::RawFd;
use std::panic::{self, AssertUnwindSafe};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::airplay2::{self, Failure};
use crate::browse;
use crate::clock::Clock;
use crate::raop;
use crate::rtsp::{Connection, OpenError, ReadError, Request};
use crate::session::{AudioLatency, Session};
use crate::turns::Turn;
use crate::{
    Browser, Counters, FRAMES_PER_PACKET, LATENCY, Protocol, Receiver, ReceiverId, Render, Status,
};
use realtime::{Realtime, Sockets};

const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
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
            playout_delay: OnceLock::new(),
        });
        let browser = browser.shared().clone();
        let turn = browser.turns.take(receiver.clone());
        let thread = settings.and_then(|settings| {
            let shared = shared.clone();
            thread::Builder::new()
                .name("airplay-stream".into())
                .spawn(move || run(&shared, &browser, &receiver, turn, settings, render))
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

    /// Codex: stops rendering and returns at once. The session keeps answering
    /// the receiver through the last packet's end plus `playout_delay`, for a
    /// receiver that sounds frames later than `Render::play_at`, then tears down.
    /// Waiting on the result returns at most `LATENCY`, `playout_delay` and the
    /// teardown timeout from now, with a poll slice or two.
    pub fn finish(mut self, playout_delay: Duration) -> Finishing {
        self.shared.playout_delay.set(playout_delay).unwrap();
        self.shared
            .command
            .store(Command::Finish as u8, Ordering::Release);
        Finishing {
            thread: self.thread.take(),
        }
    }

    /// Claude: stops rendering and sending and tears the session down without
    /// letting the receiver play out what it holds, which it may still play.
    /// Every wait the session makes from now on, connecting, sending, reading
    /// or tearing down, gives up once stopped or within the teardown timeout,
    /// so waiting on the result returns at most that and a poll slice or two
    /// from now.
    pub fn stop(mut self) -> Finishing {
        self.shared
            .command
            .store(Command::Stop as u8, Ordering::Release);
        Finishing {
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

/// Claude: a stream playing out its last packets, or tearing down. The
/// receiver is still taken until it is finished.
pub struct Finishing {
    thread: Option<JoinHandle<()>>,
}

impl Finishing {
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Codex: blocks until the session's thread has torn down and released
    /// its sockets, within the bound `finish` or `stop` gives. A renderer that
    /// blocks holds it up, and keeps the receiver taken, until it returns.
    pub fn wait(mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Shared {
    status: Mutex<Status>,
    command: AtomicU8,
    clock: Clock,
    playout_delay: OnceLock<Duration>,
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
        timing = streamer.delivery.timing(),
        audio = streamer.delivery.audio(),
        address = %streamer.session.connection.peer_addr(),
        audio_latency = streamer.delivery.audio_latency().map(tracing::field::debug),
        "AirPlay stream started"
    );
    shared.set_status(Status::Streaming(Counters::default()));
    let end = streamer.play();
    if let End::Lost(reason) = &end {
        warn!(%receiver, %reason, "AirPlay stream lost");
        shared.set_status(Status::Lost(reason.clone()));
    }
    streamer.teardown();
    info!(%receiver, ?end, counters = ?streamer.delivery.counters(), late_packets = streamer.late_packets, "AirPlay stream ended");
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
            Err(_) if now.missing(protocol) => (found, protocol),
            Err(reason) => return Err(format!("{}: {reason}", now.name)),
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
    let (session, delivery) = match connector.attempt(&found, protocol) {
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
    Ok(Streamer {
        shared,
        keepalive: Keepalive {
            next_at: Instant::now() + session.keepalive().every,
            outstanding: None,
        },
        settings,
        render,
        session,
        delivery,
        left: Box::new([0.0; FRAMES_PER_PACKET]),
        right: Box::new([0.0; FRAMES_PER_PACKET]),
        next: 0,
        sent: None,
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
    fn attempt(
        &self,
        found: &Receiver,
        protocol: Protocol,
    ) -> Result<(Session, Delivery), Failure> {
        let failed = |reason: String| Failure::Failed(reason);
        let service = found.service(protocol).map_err(failed)?;
        let connection = self
            .open(protocol, service.addresses.clone())
            .map_err(failed)?;
        let sockets = Sockets::bind(connection.local_addr(), self.clock).map_err(failed)?;
        let offer = sockets.offer(self.settings);
        let timeout = self.settings.answer_timeout;
        let (mut session, audio) = match protocol {
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
        match Realtime::start(sockets, audio, self.settings, self.clock) {
            Ok(realtime) => Ok((session, Delivery::Realtime(realtime))),
            Err(reason) => {
                session.teardown(self.settings.teardown_timeout);
                Err(failed(reason))
            }
        }
    }

    /// Claude: connects to the receiver, trying again while it fails as one
    /// waking from standby does, until `wake_window` ends: no attempt runs past
    /// it. Each try goes to the addresses browsing knows by then, since a
    /// waking receiver may come back on others, and none follows once the
    /// receiver turns the stream away.
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
            if let Some(now) = self.browser.receiver(receiver) {
                match now.service(protocol) {
                    Ok(service) => addresses = service.addresses.clone(),
                    Err(_) if now.missing(protocol) => {}
                    Err(reason) => return Err(reason),
                }
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

/// Claude: how a stream's packets reach the receiver and when each plays.
enum Delivery {
    Realtime(Realtime),
}

impl Delivery {
    fn timing(&self) -> &'static str {
        match self {
            Delivery::Realtime(_) => "NTP",
        }
    }

    fn audio(&self) -> &'static str {
        match self {
            Delivery::Realtime(_) => "realtime",
        }
    }

    /// Claude: what a realtime receiver adds to the play times its sync
    /// packets give, as it reported or as assumed.
    fn audio_latency(&self) -> Option<AudioLatency> {
        match self {
            Delivery::Realtime(realtime) => Some(realtime.audio_latency()),
        }
    }

    fn counters(&self) -> Counters {
        match self {
            Delivery::Realtime(realtime) => realtime.counters(),
        }
    }

    fn play_at(&self, packet: u64) -> Instant {
        match self {
            Delivery::Realtime(realtime) => realtime.play_at(packet),
        }
    }

    fn send(
        &mut self,
        packet: u64,
        left: &[f32; FRAMES_PER_PACKET],
        right: &[f32; FRAMES_PER_PACKET],
    ) -> Result<(), String> {
        match self {
            Delivery::Realtime(realtime) => realtime.send(packet, left, right),
        }
    }

    /// Claude: the socket whose readiness `on_ready` answers.
    fn interest(&self) -> libc::pollfd {
        match self {
            Delivery::Realtime(realtime) => realtime.interest(),
        }
    }

    fn on_ready(&mut self) {
        match self {
            Delivery::Realtime(realtime) => realtime.answer_control(),
        }
    }

    /// Claude: what the delivery does over time whether or not packets go
    /// out, and how it finds the receiver gone.
    fn upkeep(&mut self, now: Instant) -> Result<(), String> {
        match self {
            Delivery::Realtime(realtime) => realtime.upkeep(now),
        }
    }

    /// Claude: when `upkeep` next has something to do.
    fn wake_at(&self) -> Instant {
        match self {
            Delivery::Realtime(realtime) => realtime.wake_at(),
        }
    }
}

/// Claude: a session from its RECORD on: it renders each packet `LATENCY`
/// before it plays, hands it to the delivery and keeps the session alive
/// until it ends.
struct Streamer<'a, R> {
    shared: &'a Shared,
    settings: Settings,
    render: R,
    session: Session,
    delivery: Delivery,
    left: Box<[f32; FRAMES_PER_PACKET]>,
    right: Box<[f32; FRAMES_PER_PACKET]>,
    next: u64,
    /// Claude: the last packet handed to the delivery.
    sent: Option<u64>,
    keepalive: Keepalive,
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
        let mut finish_at = None;
        loop {
            let step = match self.shared.command() {
                Command::Stop => return End::Stopped,
                Command::Finish => {
                    let end = match finish_at {
                        Some(end) => end,
                        None => match self.played_out() {
                            Some(end) => *finish_at.insert(end),
                            None => return End::Finished,
                        },
                    };
                    if Instant::now() >= end {
                        return End::Finished;
                    }
                    self.wait_until(end)
                }
                Command::Play => {
                    let due = self.delivery.play_at(self.next) - self.settings.latency;
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
                return match self.shared.command() {
                    Command::Stop => End::Stopped,
                    _ => End::Lost(reason),
                };
            }
        }
    }

    /// Claude: when the last packet sent has played, plus `playout_delay`, or
    /// `None` when none was sent.
    fn played_out(&self) -> Option<Instant> {
        let last = self.sent?;
        Some(self.delivery.play_at(last + 1) + *self.shared.playout_delay.get().unwrap())
    }

    fn play_next(&mut self) -> Result<(), String> {
        let cutoff = Instant::now() + MIN_LEAD;
        let mut late = 0;
        while self.delivery.play_at(self.next) <= cutoff {
            self.next += 1;
            late += 1;
        }
        let packet = self.next;
        self.next += 1;
        let play_at = self.delivery.play_at(packet);
        self.left.fill(0.0);
        self.right.fill(0.0);
        let render = &mut self.render;
        let channels = [&mut self.left[..], &mut self.right[..]];
        panic::catch_unwind(AssertUnwindSafe(|| render(Render { channels, play_at })))
            .map_err(|payload| format!("the renderer panicked: {}", panic_message(&*payload)))?;
        if Instant::now() + MIN_LEAD < play_at {
            let sent = self.delivery.send(packet, &self.left, &self.right);
            self.sent = Some(packet);
            self.publish();
            sent?;
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

    fn publish(&self) {
        self.shared
            .set_status(Status::Streaming(self.delivery.counters()));
    }

    /// Claude: answers the receiver and keeps the delivery up until
    /// `deadline`, or until the command changes, polling once even when the
    /// deadline has passed.
    fn wait_until(&mut self, deadline: Instant) -> Result<(), String> {
        let command = self.shared.command();
        loop {
            let timeout = deadline
                .min(self.delivery.wake_at())
                .saturating_duration_since(Instant::now())
                .min(POLL_SLICE);
            let [delivery, rtsp] = poll(
                [
                    self.delivery.interest(),
                    readable(self.session.connection.fd()),
                ],
                timeout,
            );
            if delivery {
                self.delivery.on_ready();
                self.publish();
            }
            if rtsp {
                self.read_rtsp()?;
            }
            self.keep_alive()?;
            self.delivery.upkeep(Instant::now())?;
            if Instant::now() >= deadline || self.shared.command() != command {
                return Ok(());
            }
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

    fn keep_alive(&mut self) -> Result<(), String> {
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
            let shared = self.shared;
            let stopped = || shared.command() == Command::Stop;
            let deadline = now + self.settings.answer_timeout;
            let cseq = self.session.connection.send(&request, deadline, &stopped)?;
            self.keepalive.outstanding = Some((cseq, now));
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

fn readable(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

/// Claude: which of `fds` are ready for what they wait on, or have failed or
/// hung up.
fn poll<const N: usize>(mut fds: [libc::pollfd; N], timeout: Duration) -> [bool; N] {
    let ms = timeout.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), N as libc::nfds_t, ms) };
    if ready <= 0 {
        return [false; N];
    }
    fds.map(|p| p.revents & (p.events | libc::POLLHUP | libc::POLLERR) != 0)
}
