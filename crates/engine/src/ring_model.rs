//! Claude: the ring and a sender's stream clock against a reference that keeps
//! every frame with when it was captured and when its push arrived, over every
//! short interleaving of producer and consumer operations.

use std::time::{Duration, Instant};

use crate::host_time;
use crate::ring::{CHANNELS, Captured, Ring};
use crate::stream_clock::{Crossing, StreamClock};

const RATE: f64 = 48_000.0;
const CAPACITY: usize = 1 << 13;
/// Claude: the reference's seconds count from this long before a check runs,
/// so every arrival lies in the past.
const ORIGIN_AGO: f64 = 100.0;
/// Claude: a push arrives this long after its last frame was captured.
const LATE: f64 = 0.02;
/// Claude: how much capture a break skips.
const GAP: f64 = 1.0;
const TOLERANCE_SECONDS: f64 = 1e-6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Measured,
    Estimated,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Claude: a push that continues the producer's run.
    Push(Kind),
    /// Claude: a marked discontinuity, then a push captured `GAP` after the
    /// run before it ended.
    Break(Kind),
    Take,
    Available,
    CapturedAt,
    Date,
    LastPush,
    /// Claude: a sender's clock advance before a pop.
    Clock,
    /// Claude: a sender's pop of one packet, when one is there.
    Pop,
    /// Claude: Available, CapturedAt, Date and LastPush in turn.
    Look,
    /// Claude: Clock then Pop, when a packet is there.
    Packet,
}

/// Claude: one push as the reference keeps it.
struct Block {
    start: usize,
    len: usize,
    run: usize,
    /// Claude: model seconds its first frame was captured.
    captured: f64,
    arrival: f64,
    dated: bool,
}

/// Claude: the truth: every block pushed and where the consumer stands.
struct Reference {
    blocks: Vec<Block>,
    frames: usize,
    run: usize,
    next: f64,
    tail: usize,
    taken: Option<usize>,
}

impl Reference {
    fn push(&mut self, kind: Kind, broken: bool, len: usize) -> &Block {
        if self.blocks.is_empty() || broken {
            self.run += 1;
        }
        if broken && !self.blocks.is_empty() {
            self.next += GAP;
        }
        self.blocks.push(Block {
            start: self.frames,
            len,
            run: self.run,
            captured: self.next,
            arrival: self.next + len as f64 / RATE + LATE,
            dated: kind != Kind::Unknown,
        });
        self.frames += len;
        self.next += len as f64 / RATE;
        self.blocks.last().unwrap()
    }

    /// Claude: one past the last frame of the taken run.
    fn end(&self) -> usize {
        let taken = self.taken.unwrap();
        self.blocks
            .iter()
            .filter(|block| block.run <= taken)
            .map(|block| block.len)
            .sum()
    }

    fn available(&self) -> usize {
        self.taken.map_or(0, |_| self.end() - self.tail)
    }

    fn block_at(&self, frame: usize) -> &Block {
        self.blocks
            .iter()
            .find(|block| (block.start..block.start + block.len).contains(&frame))
            .unwrap()
    }

    /// Claude: when frame `frame` was captured, if its push was dated.
    fn captured(&self, frame: usize) -> Option<f64> {
        let block = self.block_at(frame);
        block
            .dated
            .then(|| block.captured + (frame - block.start) as f64 / RATE)
    }

    /// Claude: when frame `frame` was captured as its push's arrival tells.
    fn arrived(&self, frame: usize) -> f64 {
        let block = self.block_at(frame);
        block.arrival - (block.start + block.len - frame) as f64 / RATE
    }

    /// Claude: the frame at the tail, or the last of the taken run when the
    /// consumer has read it all, and how many frames the tail lies past it.
    fn at_tail(&self) -> (usize, usize) {
        let end = self.end();
        if self.tail < end {
            (self.tail, 0)
        } else {
            (end - 1, self.tail - end + 1)
        }
    }

    fn tail_captured(&self) -> Option<f64> {
        let (frame, past) = self.at_tail();
        self.captured(frame).map(|at| at + past as f64 / RATE)
    }

    fn tail_time(&self) -> f64 {
        let (frame, past) = self.at_tail();
        self.captured(frame).unwrap_or_else(|| self.arrived(frame)) + past as f64 / RATE
    }

    fn tail_arrived(&self) -> f64 {
        let (frame, past) = self.at_tail();
        self.arrived(frame) + past as f64 / RATE
    }

    fn last_push(&self) -> (usize, f64) {
        let taken = self.taken.unwrap();
        let last = self
            .blocks
            .iter()
            .rfind(|block| block.run == taken)
            .unwrap();
        (last.start + last.len - self.tail, last.arrival)
    }

    fn take(&mut self) -> Option<f64> {
        if self.blocks.is_empty() || self.taken == Some(self.run) {
            return None;
        }
        let first = self
            .blocks
            .iter()
            .find(|block| block.run == self.run)
            .unwrap();
        self.tail = first.start;
        self.taken = Some(self.run);
        Some(self.tail_time())
    }
}

/// Claude: a ring and the stream clock a sender keeps over it, against the
/// reference and its own clock.
pub struct Check {
    ring: Ring,
    clock: StreamClock,
    reference: Reference,
    expected: StreamClock,
    packet: usize,
    push: usize,
    origin: f64,
    ticks_per_second: f64,
}

/// Claude: source frames of media a crossing skips; an overlap counts back.
fn frames(crossing: &Crossing, packet: usize) -> i64 {
    match crossing {
        Crossing::Continuous => 0,
        Crossing::Gap { silence, packets } => (silence + packets * packet) as i64,
        Crossing::Overlap(frames) => -(*frames as i64),
    }
}

impl Check {
    pub fn new(packet: usize, push: usize) -> Self {
        let ticks_per_second = host_time::ticks_per_second();
        let delay = if packet == 960 { 312 } else { 2112 };
        Self {
            ring: Ring::with_capacity(RATE, CAPACITY),
            clock: StreamClock::new(RATE, RATE, packet, delay),
            reference: Reference {
                blocks: Vec::new(),
                frames: 0,
                run: 0,
                next: 0.0,
                tail: 0,
                taken: None,
            },
            expected: StreamClock::new(RATE, RATE, packet, delay),
            packet,
            push,
            origin: host_time::now() as f64 - ORIGIN_AGO * ticks_per_second,
            ticks_per_second,
        }
    }

    fn ticks(&self, seconds: f64) -> f64 {
        self.origin + seconds * self.ticks_per_second
    }

    fn seconds(&self, ticks: f64) -> f64 {
        (ticks - self.origin) / self.ticks_per_second
    }

    fn same_time(&self, what: &str, got: f64, expected: f64) -> Result<(), String> {
        let got = self.seconds(got);
        if (got - expected).abs() <= TOLERANCE_SECONDS {
            Ok(())
        } else {
            Err(format!("{what}: {got} s, expected {expected} s"))
        }
    }

    pub fn run(&mut self, op: Op) -> Result<(), String> {
        match op {
            Op::Push(kind) | Op::Break(kind) => self.push(kind, matches!(op, Op::Break(_))),
            Op::Take => self.take(),
            Op::Available => self.available().map(|_| ()),
            Op::CapturedAt => self.captured_at(),
            Op::Date => self.date(),
            Op::LastPush => self.last_push(),
            Op::Clock => self.clock(),
            Op::Pop => self.pop(),
            Op::Look => {
                self.available()?;
                self.captured_at()?;
                self.date()?;
                self.last_push()
            }
            Op::Packet => {
                if self.reference.available() >= self.packet {
                    self.clock()?;
                    self.pop()?;
                }
                Ok(())
            }
        }
    }

    fn push(&mut self, kind: Kind, broken: bool) -> Result<(), String> {
        if broken {
            self.ring.discontinuity();
        }
        let block = self.reference.push(kind, broken, self.push);
        let (start, len, captured, arrival) =
            (block.start, block.len, block.captured, block.arrival);
        let at = self.ticks(captured) as u64;
        let captured = match kind {
            Kind::Measured => Captured::Measured(at),
            Kind::Estimated => Captured::Estimated(at),
            Kind::Unknown => Captured::Unknown,
        };
        let pushed = self
            .ring
            .push_with(len, self.ticks(arrival) as u64, captured, |f, _| {
                (start + f + 1) as f32
            });
        pushed
            .then_some(())
            .ok_or_else(|| "a push did not fit".to_string())
    }

    fn take(&mut self) -> Result<(), String> {
        let got = self.ring.take_discontinuity();
        let expected = self.reference.take();
        match (got, expected) {
            (None, None) => Ok(()),
            (Some(got), Some(expected)) => {
                self.same_time("taken run's first frame", got, expected)?;
                let crossed = frames(&self.clock.cross(got), self.packet);
                let truth = frames(&self.expected.cross(self.ticks(expected)), self.packet);
                if crossed.abs_diff(truth) > 1 {
                    return Err(format!("crossing of {crossed} frames, expected {truth}"));
                }
                Ok(())
            }
            (got, expected) => Err(format!("took {got:?}, expected {expected:?}")),
        }
    }

    fn available(&mut self) -> Result<usize, String> {
        let got = self.ring.available();
        let expected = self.reference.available();
        if got == expected {
            Ok(got)
        } else {
            Err(format!("{got} frames available, expected {expected}"))
        }
    }

    /// Claude: Instant and host time are one clock, so readings of both
    /// before and after the call bound the instant the ring can return.
    fn captured_at(&mut self) -> Result<(), String> {
        let (before, host_before) = (Instant::now(), host_time::now() as f64);
        let got = self.ring.captured_at(RATE, self.ticks_per_second);
        let (after, host_after) = (Instant::now(), host_time::now() as f64);
        if self.reference.taken.is_none() || self.reference.available() == 0 {
            let age = after.duration_since(got).as_secs_f64();
            return (age <= ORIGIN_AGO + 10.0)
                .then_some(())
                .ok_or_else(|| format!("captured_at {age} s ago"));
        }
        let arrived = self.ticks(self.reference.tail_arrived());
        let age = |host: f64| Duration::from_secs_f64((host - arrived) / self.ticks_per_second);
        let slack = Duration::from_millis(1);
        let (earliest, latest) = (
            before - age(host_after) - slack,
            after - age(host_before) + slack,
        );
        (earliest..=latest)
            .contains(&got)
            .then_some(())
            .ok_or_else(|| {
                format!(
                    "captured_at {:?} before the call, not its push's arrival",
                    before.saturating_duration_since(got)
                )
            })
    }

    fn date(&mut self) -> Result<(), String> {
        if self.reference.taken.is_none() {
            return match self.ring.tail_captured(self.ticks_per_second / RATE) {
                None => Ok(()),
                Some(_) => Err("dated before any run was taken".into()),
            };
        }
        let got = self.ring.tail_captured(self.ticks_per_second / RATE);
        match (got, self.reference.tail_captured()) {
            (None, None) => {}
            (Some(got), Some(expected)) => self.same_time("tail_captured", got as f64, expected)?,
            (got, expected) => {
                return Err(format!("tail_captured {got:?}, expected {expected:?}"));
            }
        }
        let tail_time = self.reference.tail_time();
        self.same_time("tail_time", self.ring.tail_time(), tail_time)
    }

    fn last_push(&mut self) -> Result<(), String> {
        let got = self.ring.last_push();
        if self.reference.taken.is_none() {
            return (got.buffered == 0).then_some(()).ok_or_else(|| {
                format!("{} frames buffered before any run was taken", got.buffered)
            });
        }
        let (buffered, arrival) = self.reference.last_push();
        if got.buffered != buffered {
            return Err(format!(
                "last_push buffered {}, expected {buffered}",
                got.buffered
            ));
        }
        self.same_time("last_push arrival", got.at as f64, arrival)
    }

    fn clock(&mut self) -> Result<(), String> {
        if self.reference.taken.is_none() {
            return Ok(());
        }
        self.clock.popping(self.ring.tail_time(), self.packet);
        let tail = self.reference.tail_time();
        self.expected.popping(self.ticks(tail), self.packet);
        Ok(())
    }

    fn pop(&mut self) -> Result<(), String> {
        if self.available()? < self.packet {
            return Ok(());
        }
        let mut popped = vec![0.0; self.packet * CHANNELS];
        let got = self.ring.pop_into(&mut popped);
        if got != self.packet {
            return Err(format!("popped {got} frames, expected {}", self.packet));
        }
        for (offset, frame) in popped.chunks(CHANNELS).enumerate() {
            let expected = (self.reference.tail + offset + 1) as f32;
            if frame[0] != expected {
                return Err(format!(
                    "popped frame {} where {expected} was due",
                    frame[0]
                ));
            }
        }
        self.reference.tail += self.packet;
        Ok(())
    }
}

/// Claude: runs `ops` on a fresh ring; the first mismatch, with the
/// operations up to it.
pub fn check(ops: &[Op], packet: usize, push: usize) -> Result<(), String> {
    let mut check = Check::new(packet, push);
    for (at, op) in ops.iter().enumerate() {
        check.run(*op).map_err(|e| {
            format!(
                "{:?} with {packet}-frame packets and {push}-frame pushes: {e}",
                &ops[..=at]
            )
        })?;
    }
    Ok(())
}

/// Claude: every sequence of each alphabet up to `longest` operations, each
/// alphabet with its packet and push sizes on a thread of its own; returns
/// how many ran and the first few that failed.
fn every(cases: &[(Vec<Op>, usize, usize)], longest: usize) -> (usize, Vec<String>) {
    std::thread::scope(|scope| {
        let runs: Vec<_> = cases
            .iter()
            .map(|(alphabet, packet, push)| {
                let (packet, push) = (*packet, *push);
                scope.spawn(move || {
                    let (mut ran, mut failed) = (0, Vec::new());
                    let mut ops = Vec::with_capacity(longest);
                    let mut stack = vec![0usize];
                    while let Some(&next) = stack.last() {
                        if next == alphabet.len() || ops.len() == longest {
                            stack.pop();
                            ops.pop();
                            if let Some(last) = stack.last_mut() {
                                *last += 1;
                            }
                            continue;
                        }
                        ops.push(alphabet[next]);
                        ran += 1;
                        if let Err(e) = check(&ops, packet, push) {
                            if failed.len() < 3 {
                                failed.push(e);
                            }
                            ops.pop();
                            *stack.last_mut().unwrap() += 1;
                            continue;
                        }
                        stack.push(0);
                    }
                    (ran, failed)
                })
            })
            .collect();
        runs.into_iter()
            .fold((0, Vec::new()), |(ran, mut failed), run| {
                let (more, failures) = run.join().unwrap();
                failed.extend(failures);
                (ran + more, failed)
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [Kind; 3] = [Kind::Measured, Kind::Estimated, Kind::Unknown];
    const PACKETS: [usize; 2] = [960, 1024];

    fn configurations() -> Vec<(usize, usize)> {
        PACKETS
            .iter()
            .flat_map(|&packet| [(packet, 480), (packet, packet)])
            .collect()
    }

    /// Claude: report 13: the producer adds to the taken run and starts the
    /// next before the consumer looks again; the consumer then reads three
    /// packets of the old run as a sender does and crosses to the new one.
    #[test]
    fn a_run_extended_unseen_dates_every_packet_and_the_gap() {
        for kind in KINDS {
            for packet in PACKETS {
                let mut ops = vec![
                    Op::Push(kind),
                    Op::Take,
                    Op::Available,
                    Op::Push(kind),
                    Op::Push(kind),
                    Op::Break(kind),
                ];
                for _ in 0..3 {
                    ops.extend([
                        Op::Available,
                        Op::CapturedAt,
                        Op::Date,
                        Op::Clock,
                        Op::Pop,
                        Op::LastPush,
                    ]);
                }
                ops.push(Op::Take);
                check(&ops, packet, packet).unwrap();
            }
        }
    }

    /// Claude: report 11: the next run arrives between the steps of the first
    /// packet.
    #[test]
    fn the_next_run_between_the_steps_of_a_packet() {
        let steps = [Op::Available, Op::Date, Op::Clock, Op::Pop];
        for kind in KINDS {
            for packet in PACKETS {
                for at in 0..=steps.len() {
                    let mut ops = vec![Op::Push(kind), Op::Take];
                    ops.extend(&steps[..at]);
                    ops.push(Op::Break(kind));
                    ops.extend(&steps[at..]);
                    ops.extend([Op::Take, Op::Look]);
                    check(&ops, packet, packet).unwrap();
                }
            }
        }
    }

    /// Claude: every operation, every source kind mixed, up to four steps.
    #[test]
    fn every_short_interleaving_of_every_operation() {
        let mut alphabet = Vec::new();
        for kind in KINDS {
            alphabet.extend([Op::Push(kind), Op::Break(kind)]);
        }
        alphabet.extend([
            Op::Take,
            Op::Available,
            Op::CapturedAt,
            Op::Date,
            Op::LastPush,
            Op::Clock,
            Op::Pop,
        ]);
        let cases: Vec<_> = configurations()
            .into_iter()
            .map(|(packet, push)| (alphabet.clone(), packet, push))
            .collect();
        let (ran, failed) = every(&cases, 4);
        assert!(
            failed.is_empty(),
            "{} interleavings:\n{}",
            ran,
            failed.join("\n")
        );
        assert_eq!(
            ran,
            4 * (13 + 13usize.pow(2) + 13usize.pow(3) + 13usize.pow(4))
        );
    }

    fn senders_and_sources() -> Vec<(Vec<Op>, usize, usize)> {
        KINDS
            .iter()
            .flat_map(|&kind| {
                let alphabet = vec![
                    Op::Push(kind),
                    Op::Break(kind),
                    Op::Take,
                    Op::Look,
                    Op::Packet,
                ];
                configurations()
                    .into_iter()
                    .map(move |(packet, push)| (alphabet.clone(), packet, push))
            })
            .collect()
    }

    /// Claude: a sender and one kind of source, up to six steps.
    #[test]
    fn every_interleaving_of_a_sender_and_a_source() {
        let (ran, failed) = every(&senders_and_sources(), 6);
        assert!(
            failed.is_empty(),
            "{} interleavings:\n{}",
            ran,
            failed.join("\n")
        );
        assert_eq!(ran, 12 * (1..=6).map(|n| 5usize.pow(n)).sum::<usize>());
    }

    /// Claude: the same up to seven steps, a million interleavings that
    /// `just check` runs on their own rather than beside tests that play in
    /// real time.
    #[test]
    #[ignore = "exhaustive: just check runs it on its own"]
    fn every_interleaving_of_a_sender_and_a_source_up_to_seven_steps() {
        let (ran, failed) = every(&senders_and_sources(), 7);
        assert!(
            failed.is_empty(),
            "{} interleavings:\n{}",
            ran,
            failed.join("\n")
        );
        assert_eq!(ran, 12 * (1..=7).map(|n| 5usize.pow(n)).sum::<usize>());
    }
}
