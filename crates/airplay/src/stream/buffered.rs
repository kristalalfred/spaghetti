use std::io::{self, ErrorKind, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use tracing::debug;

use super::{POLL_SLICE, Settings, pause_until};
use crate::airplay2::Failure;
use crate::airplay2::buffered::{Anchor, BufferedAudio, FORMAT};
use crate::clock::{self, PACKET_DURATION};
use crate::hap::Counted;
use crate::packet::{BUFFERED_PACKET_LEN, write_buffered_packet};
use crate::ptp::{Peer, Timebase, Timeline, Timing};
use crate::{Counters, FRAMES_PER_PACKET};

/// Claude: buffered audio: each packet sealed and queued whole on the data
/// connection as it is rendered, to play on the receiver's PTP timeline from
/// the anchor it accepted. Each play time is read through the latest timebase
/// on that timeline; the stream ends once timing leaves it, since the
/// receiver holds that one anchor and is never given another.
pub(super) struct Buffered {
    peer: Peer,
    anchor: Anchor,
    timebase: Timebase,
    key: Counted,
    outbox: Outbox,
    first_seq: u32,
    first: u64,
}

impl Buffered {
    /// Claude: starts with the first packet that can still be rendered
    /// `latency` before it plays, the anchor having been accepted at
    /// `anchored_at`; the receiver plays silence before it.
    pub(super) fn start(
        audio: BufferedAudio,
        peer: Peer,
        settings: &Settings,
    ) -> Result<Buffered, String> {
        let BufferedAudio {
            data,
            key,
            anchor,
            anchored_at,
        } = audio;
        let timebase = on_timeline(anchor.timeline, peer.timing())?;
        let capacity =
            (settings.latency + Duration::from_secs(1)).as_nanos() / PACKET_DURATION.as_nanos() + 1;
        let outbox = Outbox::new(data, capacity as usize)
            .map_err(|e| format!("could not set up its data connection ({e})"))?;
        let mut buffered = Buffered {
            peer,
            anchor,
            timebase,
            key,
            outbox,
            first_seq: settings.first_seq,
            first: 0,
        };
        while buffered.play_at(buffered.first)? < anchored_at + settings.latency {
            buffered.first += 1;
        }
        debug!(
            first = buffered.first,
            ?timebase,
            "AirPlay buffered stream timed"
        );
        Ok(buffered)
    }

    pub(super) fn first_packet(&self) -> u64 {
        self.first
    }

    pub(super) fn counters(&self) -> Counters {
        self.outbox.counters()
    }

    pub(super) fn play_at(&self, packet: u64) -> Result<Instant, String> {
        self.anchor
            .network
            .after(clock::span(packet * FRAMES_PER_PACKET as u64))
            .and_then(|network| self.timebase.host(network))
            .ok_or_else(|| {
                format!("packet {packet} plays beyond what the receiver's timeline can say")
            })
    }

    pub(super) fn send(
        &mut self,
        packet: u64,
        play_at: Instant,
        left: &[f32; FRAMES_PER_PACKET],
        right: &[f32; FRAMES_PER_PACKET],
    ) -> Result<(), String> {
        let seq = self.first_seq.wrapping_add(packet as u32);
        let rtp = self
            .anchor
            .rtp
            .wrapping_add((packet * FRAMES_PER_PACKET as u64) as u32);
        let key = &mut self.key;
        let write = |out: &mut [u8; BUFFERED_PACKET_LEN]| {
            write_buffered_packet(out, seq, rtp, FORMAT.ssrc(), left, right, key)
        };
        self.outbox.push(write, play_at, Instant::now())
    }

    pub(super) fn interest(&self) -> libc::pollfd {
        self.outbox.interest()
    }

    pub(super) fn flush(&mut self, now: Instant) -> Result<(), String> {
        self.outbox.flush(now)
    }

    /// Claude: takes the latest timebase, ending the stream once timing has
    /// left the anchored timeline, and sends what the connection takes.
    pub(super) fn upkeep(&mut self, now: Instant) -> Result<(), String> {
        self.timebase = on_timeline(self.anchor.timeline, self.peer.timing())?;
        self.outbox.flush(now)
    }
}

/// Claude: waits, as long as `abort` allows, until timing with the receiver
/// has settled; timing that does not settle by `deadline`, or is lost, leaves
/// buffered audio unstarted.
pub(super) fn settle(
    peer: &Peer,
    deadline: Instant,
    abort: &dyn Fn() -> bool,
) -> Result<(), Failure> {
    loop {
        match peer.timing() {
            Timing::Settled(_) => return Ok(()),
            Timing::Lost(reason) => {
                return Err(Failure::Unbuffered(format!(
                    "its PTP timing was lost before it settled ({reason})"
                )));
            }
            Timing::Acquiring if Instant::now() >= deadline => {
                return Err(Failure::Unbuffered(
                    "its PTP timing did not settle within the startup budget".into(),
                ));
            }
            Timing::Acquiring => {
                if !pause_until((Instant::now() + POLL_SLICE).min(deadline), abort) {
                    return Err(Failure::Failed("stopped".into()));
                }
            }
        }
    }
}

/// Claude: an anchor playing the stream's first frame `latency` from now, on
/// the receiver's timeline as timing has it now.
pub(super) fn anchor_now(peer: &Peer, settings: &Settings) -> Result<Anchor, Failure> {
    let Timing::Settled(timebase) = peer.timing() else {
        return Err(Failure::Unbuffered(
            "its PTP timing was lost before it took an anchor".into(),
        ));
    };
    let network = timebase
        .network(Instant::now() + settings.latency)
        .ok_or_else(|| Failure::Unbuffered("its PTP timeline cannot say when to start".into()))?;
    Ok(Anchor {
        rtp: settings.first_rtp,
        network,
        timeline: timebase.timeline(),
    })
}

/// Claude: the receiver's timebase as timing now has it, while it is still on
/// the timeline the stream was anchored on.
fn on_timeline(anchored: Timeline, timing: Timing) -> Result<Timebase, String> {
    let now_on = match timing {
        Timing::Settled(timebase) if timebase.timeline() == anchored => return Ok(timebase),
        Timing::Settled(timebase) => timebase.timeline(),
        Timing::Acquiring => return Err("PTP timing with the receiver started over".into()),
        Timing::Lost(reason) => {
            return Err(format!("PTP timing with the receiver was lost ({reason})"));
        }
    };
    Err(if now_on.clock == anchored.clock {
        format!(
            "the receiver's clock {:016X} stepped (epoch {} to {})",
            anchored.clock.as_u64(),
            anchored.epoch,
            now_on.epoch
        )
    } else {
        format!(
            "the receiver's timeline moved from clock {:016X} to {:016X}",
            anchored.clock.as_u64(),
            now_on.clock.as_u64()
        )
    })
}

/// Claude: buffered packets on their way over the data connection, whole: a
/// packet written in part stays first until the rest of it goes, so the
/// connection's framing never breaks. It holds at most `capacity` packets,
/// and fails, before writing any more of it, once a packet is still waiting
/// at its deadline, which the receiver no longer plays it after, or once the
/// connection fails. A packet
/// counts as sent once all of it is written. A stalled receiver shows only
/// once the kernel takes no more, which on macOS is after hundreds of
/// kilobytes, its buffers growing past what is asked for.
pub(super) struct Outbox {
    socket: TcpStream,
    slots: Box<[Slot]>,
    head: usize,
    queued: usize,
    written: usize,
    counters: Counters,
}

struct Slot {
    len: usize,
    deadline: Instant,
    bytes: [u8; BUFFERED_PACKET_LEN],
}

impl Outbox {
    pub(super) fn new(socket: TcpStream, capacity: usize) -> io::Result<Outbox> {
        assert!(capacity > 0);
        socket.set_nonblocking(true)?;
        socket.set_nodelay(true)?;
        Ok(Outbox {
            socket,
            slots: (0..capacity)
                .map(|_| Slot {
                    len: 0,
                    deadline: Instant::now(),
                    bytes: [0; BUFFERED_PACKET_LEN],
                })
                .collect(),
            head: 0,
            queued: 0,
            written: 0,
            counters: Counters::default(),
        })
    }

    pub(super) fn counters(&self) -> Counters {
        self.counters
    }

    /// Claude: queues the packet `write` puts in the buffer it is given,
    /// returning its length, to be sent by `deadline`, then sends what the
    /// connection takes.
    pub(super) fn push(
        &mut self,
        write: impl FnOnce(&mut [u8; BUFFERED_PACKET_LEN]) -> usize,
        deadline: Instant,
        now: Instant,
    ) -> Result<(), String> {
        if self.queued == self.slots.len() {
            return Err(format!(
                "the receiver fell {} packets behind taking audio",
                self.queued
            ));
        }
        let tail = (self.head + self.queued) % self.slots.len();
        let slot = &mut self.slots[tail];
        slot.len = write(&mut slot.bytes);
        slot.deadline = deadline;
        self.queued += 1;
        self.flush(now)
    }

    /// Claude: the socket, writable when packets wait.
    pub(super) fn interest(&self) -> libc::pollfd {
        libc::pollfd {
            fd: if self.queued > 0 {
                self.socket.as_raw_fd()
            } else {
                -1
            },
            events: libc::POLLOUT,
            revents: 0,
        }
    }

    /// Claude: writes what the connection takes without blocking.
    pub(super) fn flush(&mut self, now: Instant) -> Result<(), String> {
        while self.queued > 0 {
            let slot = &self.slots[self.head];
            if now >= slot.deadline {
                return Err(format!(
                    "the receiver had not taken a packet {:?} after it was due to play",
                    now - slot.deadline
                ));
            }
            match self.socket.write(&slot.bytes[self.written..slot.len]) {
                Ok(0) => return Err("the receiver closed its data connection".into()),
                Ok(n) => {
                    self.written += n;
                    if self.written == slot.len {
                        self.counters.packets_sent += 1;
                        self.counters.bytes_sent += slot.len as u64;
                        self.written = 0;
                        self.head = (self.head + 1) % self.slots.len();
                        self.queued -= 1;
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("its data connection failed ({e})")),
            }
        }
        Ok(())
    }

    /// Claude: how much of the first packet waiting has gone out.
    #[cfg(test)]
    fn partly_written(&self) -> usize {
        self.written
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener};
    use std::thread;

    use socket2::{Domain, SockRef, Socket, Type};

    use super::*;
    use crate::fake::{BufferedPacket, DataReader};

    const KEY: [u8; 32] = [0x3C; 32];
    const LEN: usize = BUFFERED_PACKET_LEN;
    const SMALL_BUFFER: usize = 8 * 1024;
    /// Claude: more packets than macOS's loopback takes into its buffers
    /// before a write blocks, which it grows past what is asked for: 190 to
    /// 550 KB were seen.
    const KERNEL_FULL: usize = 1000;
    const FAR: Duration = Duration::from_secs(60);

    /// Claude: a connection whose kernel buffers start small.
    fn connection() -> (TcpStream, TcpStream) {
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        socket.set_recv_buffer_size(SMALL_BUFFER).unwrap();
        socket
            .bind(&SocketAddr::from((Ipv4Addr::LOCALHOST, 0)).into())
            .unwrap();
        socket.listen(1).unwrap();
        let listener: TcpListener = socket.into();
        let sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        SockRef::from(&sender)
            .set_send_buffer_size(SMALL_BUFFER)
            .unwrap();
        let (receiver, _) = listener.accept().unwrap();
        (sender, receiver)
    }

    /// Claude: packet `n` of a run whose frames carry `n`.
    fn packet(n: u32, key: &mut Counted) -> impl FnOnce(&mut [u8; BUFFERED_PACKET_LEN]) -> usize {
        let left = [f32::from(n as u16 as i16) / 32768.0; FRAMES_PER_PACKET];
        move |out| write_buffered_packet(out, n, n * 352, 0x1200_0000, &left, &left, key)
    }

    /// Claude: a run of `count` packets made ahead, since sealing takes a
    /// debug build long enough to eat a test's lead.
    fn run(count: usize) -> Vec<[u8; BUFFERED_PACKET_LEN]> {
        let mut key = Counted::new(&KEY);
        (0..count as u32)
            .map(|n| {
                let mut out = [0; BUFFERED_PACKET_LEN];
                packet(n, &mut key)(&mut out);
                out
            })
            .collect()
    }

    /// Claude: reads the receiving end `chunk` bytes at a time, pausing as
    /// `pause` says once `after` bytes have come, until it has `until` bytes or
    /// the connection ends, then returns the packets they hold and how many
    /// bytes of an unfinished one are left. It opens them only at the end, so
    /// that a debug build reads as fast as the connection goes.
    fn read(
        mut receiver: TcpStream,
        chunk: usize,
        pause: Option<(usize, Duration)>,
        until: usize,
    ) -> thread::JoinHandle<(Vec<BufferedPacket>, usize)> {
        thread::spawn(move || {
            let mut received = Vec::new();
            let mut pause = pause;
            let mut buffer = vec![0; chunk];
            while received.len() < until {
                if let Some((after, duration)) = pause
                    && received.len() >= after
                {
                    thread::sleep(duration);
                    pause = None;
                }
                let limit = match pause {
                    Some((after, _)) => chunk.min(after - received.len()),
                    None => chunk,
                }
                .min(until - received.len());
                match receiver.read(&mut buffer[..limit]) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => received.extend_from_slice(&buffer[..n]),
                }
            }
            let mut reader = DataReader::default();
            let packets = reader
                .feed(&received)
                .iter()
                .map(|f| f.open(&KEY))
                .collect();
            (packets, reader.held())
        })
    }

    /// Claude: flushes as the stream's loop does, every millisecond, until
    /// `until`.
    fn keep_flushing(outbox: &mut Outbox, until: Instant) -> Result<(), String> {
        while Instant::now() < until {
            thread::sleep(Duration::from_millis(1));
            outbox.flush(Instant::now())?;
        }
        Ok(())
    }

    /// Claude: flushes as the stream's loop does, every few milliseconds,
    /// until the outbox is empty, fails, or `within` ends.
    fn drain(outbox: &mut Outbox, within: Duration) -> Result<(), String> {
        let until = Instant::now() + within;
        while outbox.interest().fd >= 0 && Instant::now() < until {
            thread::sleep(Duration::from_millis(2));
            outbox.flush(Instant::now())?;
        }
        Ok(())
    }

    fn check_run(packets: &[BufferedPacket], count: usize) {
        assert_eq!(packets.len(), count);
        for (n, packet) in packets.iter().enumerate() {
            assert_eq!(packet.seq, n as u32);
            assert_eq!(packet.timestamp, n as u32 * 352);
            assert!(packet.frames.iter().all(|f| *f == [n as i16, n as i16]));
        }
    }

    #[test]
    fn a_stream_keeps_only_timing_on_the_timeline_it_was_anchored_on() {
        use crate::ptp::{ClockIdentity, PtpTime};

        let anchored = Timeline {
            clock: ClockIdentity([1; 8]),
            epoch: 4,
        };
        let on = |timeline| {
            Timing::Settled(
                Timebase::new(timeline, Instant::now(), PtpTime { nanos: 1 << 40 }, 1.0).unwrap(),
            )
        };
        assert!(on_timeline(anchored, on(anchored)).is_ok());
        let stepped = Timeline {
            epoch: 5,
            ..anchored
        };
        assert_eq!(
            on_timeline(anchored, on(stepped)).unwrap_err(),
            "the receiver's clock 0101010101010101 stepped (epoch 4 to 5)"
        );
        let other = Timeline {
            clock: ClockIdentity([2; 8]),
            epoch: 4,
        };
        assert_eq!(
            on_timeline(anchored, on(other)).unwrap_err(),
            "the receiver's timeline moved from clock 0101010101010101 to 0202020202020202"
        );
        assert_eq!(
            on_timeline(anchored, Timing::Acquiring).unwrap_err(),
            "PTP timing with the receiver started over"
        );
        assert_eq!(
            on_timeline(anchored, Timing::Lost("silent".into())).unwrap_err(),
            "PTP timing with the receiver was lost (silent)"
        );
    }

    #[test]
    fn a_reader_taking_one_byte_at_a_time_gets_every_packet_whole() {
        let (sender, receiver) = connection();
        let count = 40;
        let total = count * BUFFERED_PACKET_LEN;
        let reading = read(receiver, 1, None, total);
        let mut outbox = Outbox::new(sender, count).unwrap();
        let mut key = Counted::new(&KEY);
        for n in 0..count as u32 {
            let now = Instant::now();
            outbox.push(packet(n, &mut key), now + FAR, now).unwrap();
        }
        drain(&mut outbox, Duration::from_secs(10)).unwrap();
        let (packets, held) = reading.join().unwrap();
        check_run(&packets, count);
        assert_eq!(held, 0);
        assert_eq!(
            outbox.counters(),
            Counters {
                packets_sent: count as u64,
                packets_retransmitted: 0,
                bytes_sent: total as u64,
            }
        );
    }

    #[test]
    fn a_pause_ending_before_the_packets_are_due_delays_them_and_a_longer_one_loses_the_stream() {
        let lead = Duration::from_millis(400);
        let every = Duration::from_millis(4);
        let count = 300;
        let packets = run(count);
        let pause_after = 3000;
        for (pause, survives) in [(lead / 3, true), (Duration::from_secs(3), false)] {
            let (sender, receiver) = connection();
            let reading = read(receiver, 64 * 1024, Some((pause_after, pause)), count * LEN);
            let mut outbox = Outbox::new(sender, count).unwrap();
            let started = Instant::now();
            let mut sent = Ok(());
            for (n, packet) in packets.iter().enumerate() {
                let at = started + every * n as u32;
                let copy = |out: &mut [u8; LEN]| {
                    *out = *packet;
                    LEN
                };
                sent = sent
                    .and_then(|()| keep_flushing(&mut outbox, at))
                    .and_then(|()| outbox.push(copy, at + lead, Instant::now()));
            }
            let sent = sent.and_then(|()| drain(&mut outbox, Duration::from_secs(5)));
            if survives {
                sent.unwrap();
                check_run(&reading.join().unwrap().0, count);
            } else {
                let reason = sent.unwrap_err();
                let failed_after = started.elapsed();
                assert!(
                    reason.starts_with("the receiver had not taken a packet"),
                    "{reason}"
                );
                assert!(failed_after >= lead, "{failed_after:?}");
                assert!(failed_after < pause, "failed only after the reader read on");
                drop(outbox);
                let (packets, _) = reading.join().unwrap();
                check_run(&packets, packets.len());
            }
        }
    }

    #[test]
    fn a_reader_that_never_reads_loses_the_stream_once_a_packet_is_due_unsent() {
        let (sender, _receiver) = connection();
        let lead = Duration::from_millis(300);
        let mut outbox = Outbox::new(sender, KERNEL_FULL).unwrap();
        let mut key = Counted::new(&KEY);
        let mut first_waiting = None;
        let mut n = 0;
        let reason = loop {
            let now = Instant::now();
            if let Err(reason) = outbox.push(packet(n, &mut key), now + lead, now) {
                break reason;
            }
            if outbox.interest().fd >= 0 {
                first_waiting.get_or_insert(now);
            }
            n += 1;
            thread::sleep(Duration::from_millis(1));
        };
        let waited = first_waiting.expect("bytes waited").elapsed();
        assert!(
            reason.starts_with("the receiver had not taken a packet"),
            "{reason}"
        );
        assert!(waited >= lead, "{waited:?}");
        assert!(waited < Duration::from_secs(5), "{waited:?}");
        assert!(outbox.counters().packets_sent < u64::from(n));
    }

    #[test]
    fn packets_queued_past_the_capacity_lose_the_stream_instead_of_growing() {
        let (sender, _receiver) = connection();
        let capacity = 20;
        let mut outbox = Outbox::new(sender, capacity).unwrap();
        let mut key = Counted::new(&KEY);
        let mut accepted = 0;
        let reason = loop {
            let now = Instant::now();
            match outbox.push(packet(accepted, &mut key), now + FAR, now) {
                Ok(()) => accepted += 1,
                Err(reason) => break reason,
            }
            assert!(accepted < 1000, "the outbox grows without bound");
        };
        assert_eq!(reason, "the receiver fell 20 packets behind taking audio");
        assert_eq!(
            u64::from(accepted) - outbox.counters().packets_sent,
            capacity as u64,
            "only what the connection took left the queue"
        );
    }

    /// Claude: a connection whose sending end writes whatever room its buffer
    /// has. macOS otherwise lets a write proceed only with 2048 bytes free, so
    /// that a 1450-byte packet goes whole or not at all.
    fn connection_writing_in_part() -> (TcpStream, TcpStream) {
        let (sender, receiver) = connection();
        let one: libc::c_int = 1;
        let set = unsafe {
            libc::setsockopt(
                sender.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDLOWAT,
                (&raw const one).cast(),
                size_of_val(&one) as libc::socklen_t,
            )
        };
        assert_eq!(set, 0, "{}", io::Error::last_os_error());
        (sender, receiver)
    }

    /// Claude: pushes copies of `packets`, all due at `due`, onto a
    /// connection nobody reads until one goes out only in part, returning how
    /// many were pushed.
    fn push_until_partly_written(
        outbox: &mut Outbox,
        packets: &[[u8; LEN]],
        due: Instant,
    ) -> usize {
        for (n, packet) in packets.iter().enumerate() {
            let copy = |out: &mut [u8; LEN]| {
                *out = *packet;
                LEN
            };
            outbox.push(copy, due, Instant::now()).unwrap();
            if outbox.partly_written() > 0 {
                return n + 1;
            }
        }
        panic!("no packet went out in part");
    }

    #[test]
    fn a_packet_pushed_past_its_deadline_fails_unwritten_and_uncounted() {
        let (sender, receiver) = connection();
        let reading = read(receiver, 4096, None, LEN);
        let mut outbox = Outbox::new(sender, 4).unwrap();
        let packets = run(1);
        let copy = |out: &mut [u8; LEN]| {
            *out = packets[0];
            LEN
        };
        let now = Instant::now();
        let reason = outbox.push(copy, now, now).unwrap_err();
        assert!(
            reason.starts_with("the receiver had not taken a packet"),
            "{reason}"
        );
        assert_eq!(outbox.counters(), Counters::default());
        drop(outbox);
        assert_eq!(reading.join().unwrap(), (vec![], 0), "nothing was written");
    }

    #[test]
    fn a_packet_sent_in_part_then_due_fails_without_completing_or_splicing() {
        let (sender, receiver) = connection_writing_in_part();
        let mut outbox = Outbox::new(sender, KERNEL_FULL).unwrap();
        let due = Instant::now() + Duration::from_secs(2);
        let pushed = push_until_partly_written(&mut outbox, &run(KERNEL_FULL), due);
        let (sent, partly) = (outbox.counters().packets_sent, outbox.partly_written());
        assert!(partly > 0 && partly < LEN);
        thread::sleep(due.saturating_duration_since(Instant::now()));
        let reading = read(receiver, 64 * 1024, None, usize::MAX);
        thread::sleep(Duration::from_millis(100));
        let reason = outbox.flush(Instant::now()).unwrap_err();
        assert!(
            reason.starts_with("the receiver had not taken a packet"),
            "{reason}"
        );
        assert_eq!(
            outbox.counters().packets_sent,
            sent,
            "the late packet was not counted"
        );
        drop(outbox);
        let (packets, held) = reading.join().unwrap();
        check_run(&packets, sent as usize);
        assert_eq!(held, partly, "nothing after the late packet's first part");
        assert!(pushed > packets.len());
    }

    #[test]
    fn a_packet_sent_in_part_goes_on_whole_once_the_receiver_reads_again() {
        let (sender, receiver) = connection_writing_in_part();
        let mut outbox = Outbox::new(sender, KERNEL_FULL).unwrap();
        let pushed =
            push_until_partly_written(&mut outbox, &run(KERNEL_FULL), Instant::now() + FAR);
        let partly = outbox.partly_written();
        assert!(partly > 0 && partly < LEN);
        let reading = read(receiver, 64 * 1024, None, pushed * LEN);
        drain(&mut outbox, Duration::from_secs(10)).unwrap();
        check_run(&reading.join().unwrap().0, pushed);
        assert_eq!(outbox.counters().packets_sent, pushed as u64);
        assert_eq!(outbox.partly_written(), 0);
    }

    #[test]
    fn a_receiver_closing_mid_packet_fails_the_next_writes() {
        let (sender, receiver) = connection();
        let mut outbox = Outbox::new(sender, 50).unwrap();

        let taken = BUFFERED_PACKET_LEN + 600;
        let reading = read(receiver.try_clone().unwrap(), 4096, None, taken);
        let mut key = Counted::new(&KEY);
        let far = Instant::now() + FAR;
        outbox
            .push(packet(0, &mut key), far, Instant::now())
            .unwrap();
        outbox
            .push(packet(1, &mut key), far, Instant::now())
            .unwrap();
        let (packets, held) = reading.join().unwrap();
        check_run(&packets, 1);
        assert_eq!(held, 600, "the receiver stopped inside the second packet");
        receiver.shutdown(Shutdown::Both).unwrap();
        drop(receiver);

        let started = Instant::now();
        let mut n = 2;
        let reason = loop {
            if let Err(reason) = outbox.push(packet(n, &mut key), far, Instant::now()) {
                break reason;
            }
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "writes kept succeeding"
            );
            n += 1;
            thread::sleep(Duration::from_millis(5));
        };
        assert!(
            reason.starts_with("its data connection failed")
                || reason == "the receiver closed its data connection",
            "{reason}"
        );
    }
}
