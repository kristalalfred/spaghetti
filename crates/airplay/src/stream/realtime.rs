use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tracing::debug;

use super::Settings;
use crate::backlog::Backlog;
use crate::clock::{self, Clock, PACKET_DURATION, Timeline};
use crate::packet::{self, AudioHeader, MAX_AUDIO_PACKET_LEN, MAX_RETRANSMISSION_LEN, Sync};
use crate::session::{AudioLatency, Offer, RealtimeAudio, Resend};
use crate::{Counters, FRAMES_PER_PACKET};

const SYNC_INTERVAL: Duration = Duration::from_secs(1);

/// Claude: the UDP sockets a realtime stream sends from and answers on. They
/// are bound before the handshake, whose offer names their ports.
pub(super) struct Sockets {
    control: UdpSocket,
    audio: UdpSocket,
    timing: TimingResponder,
    control_port: u16,
    timing_port: u16,
}

impl Sockets {
    /// Claude: binds them in `local`'s address family and answers timing
    /// requests with `clock` from now on.
    pub(super) fn bind(local: SocketAddr, clock: Clock) -> Result<Sockets, String> {
        let any = match local {
            SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)),
        };
        let bind =
            || UdpSocket::bind(any).map_err(|e| format!("could not open a UDP socket ({e})"));
        let (control, timing, audio) = (bind()?, bind()?, bind()?);
        control.set_nonblocking(true).map_err(|e| e.to_string())?;
        let port = |socket: &UdpSocket| {
            socket
                .local_addr()
                .map(|a| a.port())
                .map_err(|e| e.to_string())
        };
        let (control_port, timing_port) = (port(&control)?, port(&timing)?);
        Ok(Sockets {
            timing: TimingResponder::start(timing, clock)?,
            control,
            audio,
            control_port,
            timing_port,
        })
    }

    pub(super) fn offer(&self, settings: &Settings) -> Offer {
        Offer {
            control_port: self.control_port,
            timing_port: self.timing_port,
            first_seq: settings.first_seq,
            first_rtp: settings.first_rtp,
        }
    }
}

/// Claude: realtime audio: each packet sent over UDP as it comes due, on a
/// timeline fixed in host time at the start, with sync packets mapping it to
/// the stream's NTP clock, the receiver's timing requests answered, and lost
/// packets resent from a backlog.
pub(super) struct Realtime {
    sockets: Sockets,
    receiver: RealtimeAudio,
    clock: Clock,
    timeline: Timeline,
    latency_frames: u32,
    ssrc: u32,
    backlog: Backlog,
    packet: Box<[u8; MAX_AUDIO_PACKET_LEN]>,
    retransmission: Box<[u8; MAX_RETRANSMISSION_LEN]>,
    next_sync: Instant,
    synced: bool,
    failing_since: Option<Instant>,
    send_failure_limit: Duration,
    timing_silence: Duration,
    counters: Counters,
    #[cfg(test)]
    failing_sends: Arc<AtomicBool>,
}

impl Realtime {
    /// Claude: starts the timeline so that its first packet plays one
    /// latency from now, or says why the receiver cannot take the stream.
    pub(super) fn start(
        sockets: Sockets,
        receiver: RealtimeAudio,
        settings: &Settings,
        clock: Clock,
    ) -> Result<Realtime, String> {
        let latency_frames = clock::frames(settings.latency);
        let audio_latency = receiver.latency.frames();
        if audio_latency >= latency_frames {
            return Err(format!(
                "it delays audio by {audio_latency} frames, not less than the stream's \
                 latency of {latency_frames} frames"
            ));
        }
        let ssrc = getrandom::u32().map_err(|e| e.to_string())?;
        let backlog_packets =
            (settings.latency + Duration::from_secs(1)).as_nanos() / PACKET_DURATION.as_nanos() + 1;
        let now = Instant::now();
        debug!(
            now = %clock.ntp(now),
            first_plays_at = %clock.ntp(now + settings.latency),
            first_rtp = settings.first_rtp,
            first_seq = settings.first_seq,
            latency_frames,
            audio_latency,
            "AirPlay timeline"
        );
        Ok(Realtime {
            sockets,
            receiver,
            clock,
            timeline: Timeline {
                anchor: now + settings.latency,
                first_seq: settings.first_seq,
                first_rtp: settings.first_rtp,
            },
            latency_frames,
            ssrc,
            backlog: Backlog::new(backlog_packets as usize),
            packet: Box::new([0; MAX_AUDIO_PACKET_LEN]),
            retransmission: Box::new([0; MAX_RETRANSMISSION_LEN]),
            next_sync: now,
            synced: false,
            failing_since: None,
            send_failure_limit: settings.send_failure_limit,
            timing_silence: settings.timing_silence,
            counters: Counters::default(),
            #[cfg(test)]
            failing_sends: settings.failing_sends.clone(),
        })
    }

    pub(super) fn play_at(&self, packet: u64) -> Instant {
        self.timeline.play_at(packet)
    }

    pub(super) fn audio_latency(&self) -> AudioLatency {
        self.receiver.latency
    }

    pub(super) fn counters(&self) -> Counters {
        self.counters
    }

    /// Claude: sends a packet, keeping it for retransmission even when sending
    /// fails, and gives up once every send has failed for the limit.
    pub(super) fn send(
        &mut self,
        packet: u64,
        left: &[f32; FRAMES_PER_PACKET],
        right: &[f32; FRAMES_PER_PACKET],
    ) -> Result<(), String> {
        let seq = self.timeline.seq(packet);
        let header = AudioHeader {
            seq,
            timestamp: self.timeline.rtp(packet as i64 * FRAMES_PER_PACKET as i64),
            ssrc: self.ssrc,
            marker: self.backlog.latest().is_none(),
        };
        let len = self
            .receiver
            .packets
            .write(&header, left, right, &mut self.packet);
        let sent = self.send_audio(len);
        if header.marker {
            debug!(
                seq,
                rtp = header.timestamp,
                plays_at = %self.clock.ntp(self.timeline.play_at(packet)),
                now = %self.clock.now(),
                "AirPlay first audio packet sent"
            );
        }
        self.backlog.push(packet, seq, &self.packet[..len]);
        match sent {
            Ok(sent) => {
                self.failing_since = None;
                self.counters.packets_sent += 1;
                self.counters.bytes_sent += sent as u64;
                Ok(())
            }
            Err(e) => {
                let now = Instant::now();
                let failing_for = now - *self.failing_since.get_or_insert(now);
                debug!(seq, error = %e, "could not send an audio packet");
                if failing_for >= self.send_failure_limit {
                    return Err(format!("could not send audio for {failing_for:?} ({e})"));
                }
                Ok(())
            }
        }
    }

    fn send_audio(&self, len: usize) -> std::io::Result<usize> {
        #[cfg(test)]
        if self.failing_sends.load(Ordering::Acquire) {
            return Err(std::io::Error::other("injected failure"));
        }
        self.sockets
            .audio
            .send_to(&self.packet[..len], self.receiver.audio)
    }

    /// Claude: the control socket, where retransmit requests come in.
    pub(super) fn interest(&self) -> libc::pollfd {
        libc::pollfd {
            fd: self.sockets.control.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }
    }

    pub(super) fn answer_control(&mut self) {
        let mut request = [0u8; 64];
        loop {
            let (len, from) = match self.sockets.control.recv_from(&mut request) {
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
                let resent = match self.receiver.packets.resend() {
                    Resend::Wrapped => {
                        let len = packet::write_retransmission(original, &mut self.retransmission);
                        self.sockets
                            .control
                            .send_to(&self.retransmission[..len], from)
                    }
                    Resend::Original => self.sockets.audio.send_to(original, self.receiver.audio),
                };
                if let Ok(sent) = resent {
                    self.counters.packets_retransmitted += 1;
                    self.counters.bytes_sent += sent as u64;
                }
            }
        }
    }

    /// Claude: sends a sync packet once one is due, and ends the stream once
    /// the receiver has stopped requesting timing.
    pub(super) fn upkeep(&mut self, now: Instant) -> Result<(), String> {
        self.sync_if_due(now);
        if let Some(last) = self.sockets.timing.last_request()
            && now.saturating_duration_since(last) > self.timing_silence
        {
            return Err("it stopped requesting timing".into());
        }
        Ok(())
    }

    /// Claude: when the next sync packet is due.
    pub(super) fn wake_at(&self) -> Instant {
        self.next_sync
    }

    fn sync_if_due(&mut self, now: Instant) {
        if now < self.next_sync {
            return;
        }
        self.next_sync = now + SYNC_INTERVAL;
        let frame = self.timeline.frame_playing(now);
        let sync = Sync {
            first: !self.synced,
            playing: self
                .timeline
                .rtp(frame + i64::from(self.receiver.latency.frames())),
            at: self.clock.ntp(self.timeline.frame_at(frame)),
            sending: self.timeline.rtp(frame + i64::from(self.latency_frames)),
        };
        self.synced = true;
        if let Err(e) = self
            .sockets
            .control
            .send_to(&packet::sync_packet(&sync), self.receiver.control)
        {
            debug!(error = %e, "could not send a sync packet");
        }
        debug!(
            first = sync.first,
            playing = sync.playing,
            at = %sync.at,
            sending = sync.sending,
            now = %self.clock.ntp(now),
            after_timing = self.sockets.timing.last_request().is_some(),
            "AirPlay sync sent"
        );
    }
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
                    let mut answered = 0u64;
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
                        let replied = clock.now();
                        let reply = packet::timing_reply(origin, received, replied);
                        if let Err(e) = socket.send_to(&reply, from) {
                            debug!(error = %e, "could not answer a timing request");
                        }
                        last_request
                            .swap(started.elapsed().as_nanos() as u64 + 1, Ordering::Release);
                        debug!(
                            %from,
                            request = answered,
                            receiver_sent = %origin,
                            %received,
                            %replied,
                            receiver_behind_ms = (origin.0 != 0)
                                .then(|| format!("{:.3}", received.ms_after(origin))),
                            "AirPlay timing request answered"
                        );
                        answered += 1;
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
