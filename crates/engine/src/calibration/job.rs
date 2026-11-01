//! Claude: one calibration's run: its two legs, its recording and its trains,
//! driven by the time each tick gives it, so tests can drive it in time of
//! their own.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use crate::host_time;
use crate::input::InputSource;
use crate::program_io::{InputShared, OutputShared};
use crate::reservations::Reservation;
use crate::ring::Ring;
use crate::route_output::{Output, Target};

use super::detect::Detector;
use super::emitter::{self, Emitter};
use super::probe::{TRAIN, TRAIN_SECONDS};
use super::recorder::Recorder;
use super::{Calibrated, CalibrationFailure, CalibrationState, RecordingFailure, Residual, Role};

const TICK: Duration = Duration::from_millis(5);
/// Claude: how long a leg's output plays before its first train, so that
/// whatever it does as it starts is over.
const SETTLE: f64 = 1.0;
/// Claude: how far ahead of the frames pushed a train is scheduled.
const AHEAD: f64 = 0.05;
/// Claude: how long before its schedule and after its last probe a train
/// is searched for.
const EARLY: f64 = 0.5;
const LATE: f64 = 2.0;
/// Claude: how long a leg may take to play, and the recording to deliver.
const READY_LIMIT: f64 = 25.0;
/// Claude: how long the recording may pause once it delivered.
const STALL: f64 = 0.5;
/// Claude: how far behind the host clock the recording may date its audio:
/// each train must be recorded through this long after it ends, and the
/// recording may hold this much more than was captured since the earliest
/// audio a train needs.
pub(super) const RECORDING_LAG: f64 = 2.0;
const SEQUENCE: [Role; 3] = [Role::Reference, Role::Output, Role::Reference];

/// Claude: one destination as the calibration plays to it.
pub struct Leg {
    role: Role,
    target: Option<Target>,
    delay: f64,
    latency_offset_ms: i32,
    /// Claude: the program output it plays to, whose removal ends the job.
    program: Option<Arc<OutputShared>>,
    playing: Option<Playing>,
    running_since: Option<u64>,
}

struct Playing {
    output: Output,
    ring: Arc<Ring>,
    emitter: Emitter,
    volume: Arc<AtomicU32>,
}

impl Leg {
    pub fn new(role: Role, target: Target, delay: f64, latency_offset_ms: Option<i32>) -> Self {
        let program = match &target {
            Target::Program { output, .. } => Some(Arc::clone(output)),
            _ => None,
        };
        Self {
            role,
            target: Some(target),
            delay,
            latency_offset_ms: latency_offset_ms.unwrap_or(0),
            program,
            playing: None,
            running_since: None,
        }
    }

    fn removed(&self) -> bool {
        self.program.as_ref().is_some_and(|output| output.ended())
    }

    fn open(&mut self, now: u64) -> Result<(), CalibrationFailure> {
        let Some(target) = self.target.take() else {
            return Ok(());
        };
        let volume = Arc::new(AtomicU32::new(1f32.to_bits()));
        let unavailable = |reason: String| CalibrationFailure::Unavailable {
            role: self.role,
            reason,
        };
        let opened = Output::open(target, emitter::timing(), Arc::clone(&volume));
        if self.removed() {
            return Err(CalibrationFailure::EndpointRemoved);
        }
        let (output, ring) = opened.map_err(unavailable)?;
        output.start().map_err(unavailable)?;
        self.playing = Some(Playing {
            output,
            emitter: Emitter::new(Arc::clone(&ring), now),
            ring,
            volume,
        });
        Ok(())
    }

    fn glitches(&self) -> u32 {
        self.playing.as_ref().map_or(0, |playing| {
            playing.output.glitches(&playing.ring).unwrap_or(0)
        })
    }

    #[cfg(test)]
    pub fn output(&self) -> Option<&Output> {
        self.playing.as_ref().map(|playing| &playing.output)
    }
}

/// Claude: what the job records through.
pub enum Feed {
    /// Claude: an input device, which the job captures itself.
    Input(InputSource),
    /// Claude: a program input, which feeds the job a ring it leases.
    Program(Arc<InputShared>),
}

struct Listening {
    recorder: Recorder,
    sample_rate: f64,
    /// Claude: the input's sink slot or the program input's lease.
    sink: u64,
}

/// Claude: a train: whose, when each probe was scheduled to sound, the stretch
/// of recording searched for it, and the leg's glitches as it was scheduled.
struct Train {
    role: Role,
    sound_at: [f64; TRAIN.len()],
    from: u64,
    to: u64,
    glitches: u32,
}

pub enum Step {
    Running(CalibrationState),
    Done(Result<Calibrated, CalibrationFailure>),
}

/// Claude: where a test can hold the job up, each passed once.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Analyzing,
    Releasing,
}

pub struct Job {
    /// Claude: the reference and the output, in that order.
    legs: [Leg; 2],
    feed: Feed,
    listening: Option<Listening>,
    trains: Vec<Train>,
    started: u64,
    last_heard: Option<u64>,
    alive: Arc<AtomicBool>,
    reservation: Option<Reservation>,
    /// Claude: while set, opening waits, as for a device slow to open.
    #[cfg(test)]
    pub hold: Option<Arc<AtomicBool>>,
    #[cfg(test)]
    pub at: Option<Box<dyn Fn(Phase) + Send>>,
}

impl Job {
    pub fn new(
        reference: Leg,
        output: Leg,
        feed: Feed,
        reservation: Reservation,
        alive: Arc<AtomicBool>,
    ) -> Self {
        Self {
            legs: [reference, output],
            feed,
            listening: None,
            trains: Vec::new(),
            started: 0,
            last_heard: None,
            alive,
            reservation: Some(reservation),
            #[cfg(test)]
            hold: None,
            #[cfg(test)]
            at: None,
        }
    }

    pub fn state(&self) -> CalibrationState {
        CalibrationState::Waiting(SEQUENCE[0])
    }

    fn leg(&self, role: Role) -> &Leg {
        &self.legs[index(role)]
    }

    fn leg_mut(&mut self, role: Role) -> &mut Leg {
        &mut self.legs[index(role)]
    }

    /// Claude: opens, then ticks until done, reporting each state.
    pub fn run(
        &mut self,
        cancelled: &dyn Fn() -> bool,
        report: &dyn Fn(CalibrationState),
        pause: &dyn Fn(Duration),
    ) -> Result<Calibrated, CalibrationFailure> {
        self.open(host_time::now(), cancelled)?;
        loop {
            if cancelled() {
                return Err(CalibrationFailure::Cancelled);
            }
            match self.tick(host_time::now()) {
                Step::Running(state) => report(state),
                Step::Done(result) => return result,
            }
            pause(TICK);
        }
    }

    /// Claude: starts the recording, then both legs, which play dated
    /// silence from `now` on.
    pub fn open(
        &mut self,
        now: u64,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), CalibrationFailure> {
        #[cfg(test)]
        while self
            .hold
            .as_ref()
            .is_some_and(|hold| hold.load(Ordering::Acquire))
        {
            if cancelled() {
                return Err(CalibrationFailure::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        self.started = now;
        let unavailable =
            |reason| CalibrationFailure::Recording(RecordingFailure::Unavailable(reason));
        let listening = match &mut self.feed {
            Feed::Input(input) => {
                let sample_rate = input.timing().map_err(unavailable)?.sample_rate;
                let ring = Arc::new(Ring::new(sample_rate));
                let sink = input.add_sink(Arc::clone(&ring)).map_err(unavailable)?;
                Listening {
                    recorder: Recorder::new(ring, sample_rate),
                    sample_rate,
                    sink: sink as u64,
                }
            }
            Feed::Program(input) => {
                let sample_rate = input.sample_rate();
                let ring = Arc::new(Ring::new(sample_rate));
                let sink = input.lease_sink(Arc::clone(&ring)).map_err(|reason| {
                    if input.ended() {
                        CalibrationFailure::EndpointRemoved
                    } else {
                        unavailable(reason)
                    }
                })?;
                Listening {
                    recorder: Recorder::new(ring, sample_rate),
                    sample_rate,
                    sink,
                }
            }
        };
        self.listening = Some(listening);
        for leg in &mut self.legs {
            if cancelled() {
                return Err(CalibrationFailure::Cancelled);
            }
            leg.open(now)?;
        }
        Ok(())
    }

    pub fn tick(&mut self, now: u64) -> Step {
        match self.advance(now) {
            Ok(Some(state)) => Step::Running(state),
            Ok(None) => Step::Done(self.analyze()),
            Err(failure) => Step::Done(Err(failure)),
        }
    }

    /// Claude: None once every train was recorded through.
    fn advance(&mut self, now: u64) -> Result<Option<CalibrationState>, CalibrationFailure> {
        let ticks = host_time::ticks_per_second();
        let seconds = |from: u64| now.saturating_sub(from) as f64 / ticks;
        if self.removed() {
            return Err(CalibrationFailure::EndpointRemoved);
        }
        for playing in self.legs.iter_mut().filter_map(|leg| leg.playing.as_mut()) {
            playing.emitter.feed(now);
        }

        let listening = self.listening.as_mut().expect("opened before ticking");
        if listening.recorder.drain() > 0 {
            self.last_heard = Some(now);
        }
        if listening.recorder.has_ended() {
            return Err(match &self.feed {
                Feed::Input(input) => CalibrationFailure::Recording(RecordingFailure::Unavailable(
                    input
                        .failure()
                        .unwrap_or_else(|| "the input device ended".into()),
                )),
                Feed::Program(_) => CalibrationFailure::EndpointRemoved,
            });
        }
        let quiet = match self.last_heard {
            Some(heard) => seconds(heard) > STALL,
            None => seconds(self.started) > READY_LIMIT,
        };
        if quiet {
            return Err(CalibrationFailure::Recording(RecordingFailure::Stalled));
        }
        let needed = self
            .trains
            .first()
            .map_or(now.saturating_sub((EARLY * ticks) as u64), |train| {
                train.from
            });
        listening.recorder.forget_before(needed);
        let track = listening.recorder.track();
        let held = track.frames_since(needed) as f64 / listening.sample_rate;
        let overdue = self.trains.iter().any(|train| {
            seconds(train.to) > RECORDING_LAG && track.reached().is_none_or(|end| end <= train.to)
        });
        if overdue || held > seconds(needed) + RECORDING_LAG {
            return Err(CalibrationFailure::Recording(track.falling_behind()));
        }

        for leg in &mut self.legs {
            let Some(playing) = &leg.playing else {
                continue;
            };
            if let Some(reason) = playing.output.unavailable() {
                return Err(CalibrationFailure::Unavailable {
                    role: leg.role,
                    reason,
                });
            }
            if playing.output.playing_cycles().is_some() {
                leg.running_since.get_or_insert(now);
            }
        }
        for train in self.trains.iter().filter(|train| now <= train.to) {
            if self.leg(train.role).glitches() > train.glitches {
                return Err(CalibrationFailure::Glitched { role: train.role });
            }
        }

        if let Some(&role) = SEQUENCE.get(self.trains.len()) {
            let leg = self.leg(role);
            match leg.running_since {
                Some(since) => {
                    let delay = leg.delay * ticks;
                    let mut earliest =
                        (now as f64 + AHEAD * ticks).max(since as f64 + SETTLE * ticks);
                    if let Some(previous) = self.trains.last() {
                        earliest = earliest.max(previous.to as f64 - delay + EARLY * ticks);
                    }
                    let glitches = leg.glitches();
                    let playing = self
                        .leg_mut(role)
                        .playing
                        .as_mut()
                        .expect("running legs play");
                    let first = playing
                        .emitter
                        .schedule(earliest as u64)
                        .expect("scheduled ahead of what was pushed")
                        as f64;
                    let sound_at = TRAIN.map(|offset| {
                        first + (offset * emitter::RATE).round() / emitter::RATE * ticks + delay
                    });
                    self.trains.push(Train {
                        role,
                        sound_at,
                        from: (sound_at[0] - EARLY * ticks) as u64,
                        to: (sound_at[0] + (TRAIN_SECONDS + LATE) * ticks) as u64,
                        glitches,
                    });
                }
                None if seconds(self.started) > READY_LIMIT => {
                    return Err(CalibrationFailure::Unavailable {
                        role,
                        reason: format!("it did not start playing within {READY_LIMIT} s"),
                    });
                }
                None => {}
            }
        }

        if let Some(train) = self.trains.iter().find(|train| train.to > now) {
            return Ok(Some(if now < train.from {
                CalibrationState::Waiting(train.role)
            } else {
                CalibrationState::Probing(train.role)
            }));
        }
        if let Some(&role) = SEQUENCE.get(self.trains.len()) {
            return Ok(Some(CalibrationState::Waiting(role)));
        }
        let last = self.trains.last().expect("every train scheduled").to;
        let recorded = listening_end(self.listening.as_ref()).is_some_and(|end| end > last);
        Ok((!recorded).then_some(CalibrationState::Analyzing))
    }

    /// Claude: whether the router, or a program endpoint the job uses, went
    /// away.
    fn removed(&self) -> bool {
        !self.alive.load(Ordering::Acquire)
            || self.legs.iter().any(Leg::removed)
            || matches!(&self.feed, Feed::Program(input) if input.ended())
    }

    fn analyze(&self) -> Result<Calibrated, CalibrationFailure> {
        #[cfg(test)]
        if let Some(at) = &self.at {
            at(Phase::Analyzing);
        }
        let listening = self.listening.as_ref().expect("opened before analyzing");
        let mut detector = Detector::new(listening.sample_rate);
        let mut measured = Vec::with_capacity(self.trains.len());
        for train in &self.trains {
            let span = listening
                .recorder
                .track()
                .span(train.from, train.to)
                .expect("recorded through every train")
                .map_err(CalibrationFailure::Recording)?;
            let train_measured = detector.measure(&span, train.sound_at).map_err(|failure| {
                CalibrationFailure::Probes {
                    role: train.role,
                    failure,
                }
            })?;
            measured.push(train_measured);
        }
        let [first, output, second] = measured[..] else {
            unreachable!("three trains");
        };
        let along = (output.at - first.at) / (second.at - first.at);
        let reference = first.residual + (second.residual - first.residual) * along;
        let offset = f64::from(self.leg(Role::Output).latency_offset_ms)
            + (output.residual - reference) * 1000.0;
        Ok(Calibrated {
            output: Residual {
                late_ms: output.residual * 1000.0,
                spread_ms: output.spread * 1000.0,
            },
            reference: Residual {
                late_ms: reference * 1000.0,
                spread_ms: first.spread.max(second.spread) * 1000.0,
            },
            latency_offset_ms: offset.round() as i32,
        })
    }

    /// Claude: releases everything, then gives `result`, unless by then the
    /// calibration was cancelled or something it used went away.
    pub fn conclude(
        &mut self,
        result: Result<Calibrated, CalibrationFailure>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Calibrated, CalibrationFailure> {
        self.release();
        match result {
            Ok(_) if cancelled() => Err(CalibrationFailure::Cancelled),
            Ok(_) if self.removed() => Err(CalibrationFailure::EndpointRemoved),
            result => result,
        }
    }

    /// Claude: stops the recording and the emitters, withdraws the legs from
    /// program outputs, closes device outputs and stops AirPlay streams, then
    /// lets go of what the job reserved.
    fn release(&mut self) {
        if let Some(listening) = self.listening.take() {
            match &mut self.feed {
                Feed::Input(input) => input.remove_sink(listening.sink as usize),
                Feed::Program(input) => input.withdraw_sink(listening.sink),
            }
        }
        #[cfg(test)]
        if let Some(at) = &self.at {
            at(Phase::Releasing);
        }
        let order = |output: &Output| match output {
            Output::Program(_) => 0,
            Output::Device(_) | Output::Srt(_) | Output::WebRtc(_) => 1,
            Output::AirPlay(_) => 2,
        };
        let mut playing: Vec<Playing> = self
            .legs
            .iter_mut()
            .filter_map(|leg| leg.playing.take())
            .collect();
        playing.sort_by_key(|playing| order(&playing.output));
        for Playing { output, volume, .. } in playing {
            output.close(&volume);
        }
        self.reservation = None;
    }

    #[cfg(test)]
    pub fn legs(&self) -> &[Leg; 2] {
        &self.legs
    }

    /// Claude: when each train's search ends, in seconds from opening.
    #[cfg(test)]
    pub fn train_ends(&self) -> Vec<f64> {
        let ticks = host_time::ticks_per_second();
        self.trains
            .iter()
            .map(|train| (train.to - self.started) as f64 / ticks)
            .collect()
    }

    #[cfg(test)]
    pub fn held_seconds(&self) -> f64 {
        self.listening.as_ref().map_or(0.0, |listening| {
            listening.recorder.track().held_frames() as f64 / listening.sample_rate
        })
    }
}

fn index(role: Role) -> usize {
    match role {
        Role::Reference => 0,
        Role::Output => 1,
    }
}

fn listening_end(listening: Option<&Listening>) -> Option<u64> {
    listening?.recorder.track().reached()
}
