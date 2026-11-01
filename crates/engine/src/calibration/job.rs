//! Claude: one calibration's run: its two legs, its recording and its trains,
//! driven by the time each tick gives it, so tests can drive it in time of
//! their own.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use crate::airplay_sender::AirPlayOutput;
use crate::host_time;
use crate::input::InputSource;
use crate::program_io::{InputShared, OutputShared};
use crate::reservations::Reservation;
use crate::ring::Ring;
use crate::route_output::{Output, Target};
use crate::{audio, device_output};

use super::detect::{Detector, Measured};
use super::emitter::{self, Emitter};
use super::probe::{MIN_RECORDING_RATE, PROBES, TRAIN_SECONDS, layout};
use super::recorder::Recorder;
use super::{
    Calibrated, CalibrationFailure, CalibrationState, Movement, ProbeFailure, RecordingFailure,
    Residual, Role,
};

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
/// Claude: the most a train's arrivals may drift within it, in seconds a
/// second, for its leg to count as settled. This and `STEADY_SPAN` are
/// acceptance gates chosen from one receiver's settling curve, not accuracy
/// bounds. Only an output settling exponentially with a time constant of at
/// most about a minute, as that receiver did, is within about 1.2 ms of where
/// it settles when it drifts this little (the drift times the time
/// constant). A slower one passes further off: 10 ms settling with a time
/// constant of 600 s passes both gates about 9.7 ms from where it ends.
pub(super) const STEADY_DRIFT: f64 = 0.000_02;
/// Claude: how far apart two successive output trains' lateness against the
/// reference may lie, about 10.6 s apart, for the output to count as
/// settled: under the same model, within about 1.3 ms of where it settles.
pub(super) const STEADY_SPAN: f64 = 0.000_25;
/// Claude: how far apart, in seconds, successive output trains sound: two
/// trains' searches, early, probes and late.
pub(super) const OUTPUT_TRAINS_APART: f64 = 2.0 * (EARLY + TRAIN_SECONDS + LATE);
/// Claude: how many output trains, each between two reference trains, a
/// calibration plays before it stops waiting for the output to settle.
const MOST_OUTPUT_TRAINS: usize = 5;
const MOST_TRAINS: usize = 2 * MOST_OUTPUT_TRAINS + 1;

/// Claude: reference and output trains take turns, a reference first.
fn role_of(train: usize) -> Role {
    if train.is_multiple_of(2) {
        Role::Reference
    } else {
        Role::Output
    }
}

/// Claude: what a leg plays to as the router checked it, before anything is
/// looked up, built or opened for it.
pub enum Planned {
    Device { uid: String },
    AirPlay { receiver: airplay::ReceiverId },
    Program { output: Arc<OutputShared> },
}

/// Claude: one destination as the calibration plays to it.
pub struct Leg {
    role: Role,
    planned: Option<Planned>,
    target: Option<Target>,
    delay_ms: u32,
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
    pub fn new(
        role: Role,
        planned: Planned,
        delay_ms: u32,
        latency_offset_ms: Option<i32>,
    ) -> Self {
        let program = match &planned {
            Planned::Program { output } => Some(Arc::clone(output)),
            _ => None,
        };
        Self {
            role,
            planned: Some(planned),
            target: None,
            delay_ms,
            delay: f64::from(delay_ms) / 1000.0,
            latency_offset_ms: latency_offset_ms.unwrap_or(0),
            program,
            playing: None,
            running_since: None,
        }
    }

    fn removed(&self) -> bool {
        self.program.as_ref().is_some_and(|output| output.ended())
    }

    /// Claude: finds the device, checks the schedule as a route on the
    /// emitter's timing would be checked, and builds what plays it, opening
    /// nothing.
    fn resolve(&mut self) -> Result<(), CalibrationFailure> {
        let Some(planned) = self.planned.take() else {
            return Ok(());
        };
        let timing = emitter::timing();
        let (delay, offset) = (self.delay, Some(self.latency_offset_ms));
        let target = match planned {
            Planned::Device { uid } => audio::find_device_by_uid(&uid)
                .ok_or_else(|| format!("device '{uid}' not found"))
                .and_then(|device| {
                    device_output::check_schedule(device, &timing, delay, offset)?;
                    Ok(Target::Device {
                        device,
                        delay: Some(delay),
                        latency_offset_ms: offset,
                    })
                }),
            Planned::AirPlay { receiver } => AirPlayOutput::playout(&timing, self.delay_ms, offset)
                .map(|playout| Target::AirPlay {
                    receiver,
                    playout: Box::new(playout),
                }),
            Planned::Program { output } => {
                output
                    .check_schedule(&timing, delay, offset)
                    .map(|()| Target::Program {
                        output,
                        delay: Some(delay),
                        latency_offset_ms: offset,
                    })
            }
        };
        self.target = Some(target.map_err(|reason| CalibrationFailure::Unavailable {
            role: self.role,
            reason,
        })?);
        Ok(())
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
            emitter: Emitter::new(Arc::clone(&ring), now, self.role),
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
    /// Claude: an input device, which the job finds and captures itself.
    Input {
        device_uid: String,
        source: Option<InputSource>,
    },
    /// Claude: a program input, which feeds the job a ring it leases.
    Program(Arc<InputShared>),
}

struct Listening {
    recorder: Recorder,
    sample_rate: f64,
    /// Claude: each role's detector, by `index`, so that each train is
    /// searched for with its own leg's probe and layout only.
    detectors: [Detector; 2],
    /// Claude: the input's sink slot or the program input's lease.
    sink: u64,
}

/// Claude: a train: whose, when each probe was scheduled to sound, the stretch
/// of recording searched for it, the leg's glitches as it was scheduled, and
/// what it measured once recorded through.
struct Train {
    role: Role,
    sound_at: [f64; PROBES],
    from: u64,
    to: u64,
    glitches: u32,
    outcome: Option<Outcome>,
}

/// Claude: a measured train: where it arrived, or, when its leg's timing
/// moved while it played, how fast.
#[derive(Debug, Clone, Copy)]
enum Outcome {
    Steady(Measured),
    Moving { ms_per_second: f64 },
}

impl Outcome {
    fn steady(&self) -> Option<&Measured> {
        match self {
            Outcome::Steady(measured) => Some(measured),
            Outcome::Moving { .. } => None,
        }
    }
}

pub enum Step {
    Running(CalibrationState),
    Done(Result<Calibrated, CalibrationFailure>),
}

enum Advance {
    Running(CalibrationState),
    Done(Calibrated),
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
        CalibrationState::Waiting(role_of(0))
    }

    fn leg(&self, role: Role) -> &Leg {
        &self.legs[index(role)]
    }

    fn leg_mut(&mut self, role: Role) -> &mut Leg {
        &mut self.legs[index(role)]
    }

    /// Claude: prepares and opens, then ticks until done, reporting each
    /// state.
    pub fn run(
        &mut self,
        cancelled: &dyn Fn() -> bool,
        report: &dyn Fn(CalibrationState),
        pause: &dyn Fn(Duration),
    ) -> Result<Calibrated, CalibrationFailure> {
        self.prepare(cancelled)?;
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

    /// Claude: finds and checks what both legs play to and the device the
    /// recording captures, opening nothing.
    pub fn prepare(&mut self, cancelled: &dyn Fn() -> bool) -> Result<(), CalibrationFailure> {
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
        for leg in &mut self.legs {
            if cancelled() {
                return Err(CalibrationFailure::Cancelled);
            }
            leg.resolve()?;
        }
        if cancelled() {
            return Err(CalibrationFailure::Cancelled);
        }
        let unavailable =
            |reason| CalibrationFailure::Recording(RecordingFailure::Unavailable(reason));
        if let Feed::Input { device_uid, source } = &mut self.feed
            && source.is_none()
        {
            *source = Some(InputSource::create(device_uid).map_err(unavailable)?);
        }
        let sample_rate = match &mut self.feed {
            Feed::Input {
                source: Some(input),
                ..
            } => input.timing().map_err(unavailable)?.sample_rate,
            Feed::Input { source: None, .. } => unreachable!("created above"),
            Feed::Program(input) => input.sample_rate(),
        };
        if sample_rate < MIN_RECORDING_RATE {
            return Err(unavailable(format!(
                "it records at {sample_rate} Hz, below the {MIN_RECORDING_RATE} Hz the probes need"
            )));
        }
        Ok(())
    }

    /// Claude: starts the recording, then both legs, which play dated
    /// silence from `now` on.
    pub fn open(
        &mut self,
        now: u64,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), CalibrationFailure> {
        self.started = now;
        let unavailable =
            |reason| CalibrationFailure::Recording(RecordingFailure::Unavailable(reason));
        let listening = match &mut self.feed {
            Feed::Input { source: None, .. } => {
                return Err(unavailable("the input device was not prepared".into()));
            }
            Feed::Input {
                source: Some(input),
                ..
            } => {
                let sample_rate = input.timing().map_err(unavailable)?.sample_rate;
                let ring = Arc::new(Ring::new(sample_rate));
                let sink = input.add_sink(Arc::clone(&ring)).map_err(unavailable)?;
                Listening {
                    recorder: Recorder::new(ring, sample_rate),
                    sample_rate,
                    detectors: detectors(sample_rate),
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
                    detectors: detectors(sample_rate),
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
            Ok(Advance::Running(state)) => Step::Running(state),
            Ok(Advance::Done(calibrated)) => Step::Done(Ok(calibrated)),
            Err(failure) => Step::Done(Err(failure)),
        }
    }

    fn advance(&mut self, now: u64) -> Result<Advance, CalibrationFailure> {
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
                Feed::Input { source, .. } => {
                    CalibrationFailure::Recording(RecordingFailure::Unavailable(
                        source
                            .as_ref()
                            .and_then(InputSource::failure)
                            .unwrap_or_else(|| "the input device ended".into()),
                    ))
                }
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
        let pending = || self.trains.iter().filter(|train| train.outcome.is_none());
        let needed = pending()
            .next()
            .map_or(now.saturating_sub((EARLY * ticks) as u64), |train| {
                train.from
            });
        listening.recorder.forget_before(needed);
        let track = listening.recorder.track();
        let held = track.frames_since(needed) as f64 / listening.sample_rate;
        let overdue = pending().any(|train| {
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

        self.measure_recorded()?;
        if let Some(concluded) = self.concluded(now) {
            return concluded.map(Advance::Done);
        }

        let next = self.trains.len();
        let due = next < MOST_TRAINS && self.trains.last().is_none_or(|last| now >= last.from);
        if due {
            let role = role_of(next);
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
                    let sound_at = layout(role).map(|offset| {
                        first + (offset * emitter::RATE).round() / emitter::RATE * ticks + delay
                    });
                    self.trains.push(Train {
                        role,
                        sound_at,
                        from: (sound_at[0] - EARLY * ticks) as u64,
                        to: (sound_at[0] + (TRAIN_SECONDS + LATE) * ticks) as u64,
                        glitches,
                        outcome: None,
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
            return Ok(Advance::Running(if now < train.from {
                CalibrationState::Waiting(train.role)
            } else {
                CalibrationState::Probing(train.role)
            }));
        }
        if self.trains.len() < MOST_TRAINS {
            return Ok(Advance::Running(CalibrationState::Waiting(role_of(
                self.trains.len(),
            ))));
        }
        Ok(Advance::Running(CalibrationState::Analyzing))
    }

    /// Claude: measures each train once the recording passed its end, in
    /// order. A train whose leg's timing moved while it played is kept as
    /// such, for the calibration to wait on; any other failure ends it.
    fn measure_recorded(&mut self) -> Result<(), CalibrationFailure> {
        let Self {
            trains, listening, ..
        } = self;
        let listening = listening.as_mut().expect("opened before measuring");
        for (number, train) in trains.iter_mut().enumerate() {
            if train.outcome.is_some() {
                continue;
            }
            let track = listening.recorder.track();
            let Some(span) = track.span(train.from, train.to) else {
                return Ok(());
            };
            let _train = tracing::debug_span!("train", role = %train.role, number).entered();
            let span = span.map_err(CalibrationFailure::Recording)?;
            let detector = &mut listening.detectors[index(train.role)];
            train.outcome = Some(match detector.measure(&span, train.sound_at) {
                Ok(measured) => Outcome::Steady(measured),
                Err(ProbeFailure::Moved { ms_per_second, .. }) => Outcome::Moving { ms_per_second },
                Err(failure) => {
                    return Err(CalibrationFailure::Probes {
                        role: train.role,
                        failure,
                    });
                }
            });
        }
        Ok(())
    }

    /// Claude: the calibration, once its latest two output trains and the
    /// reference trains around them are steady and agree; its failure, once
    /// it measured every train it may play and they did not.
    fn concluded(&self, now: u64) -> Option<Result<Calibrated, CalibrationFailure>> {
        let measured: Vec<&Outcome> = self
            .trains
            .iter()
            .map_while(|train| train.outcome.as_ref())
            .collect();
        let output = match measured.len() {
            n if n >= 5 && n % 2 == 1 => n - 2,
            n if n >= 5 => n - 3,
            _ => return None,
        };
        let window: Option<Vec<&Measured>> = (output - 3..=output + 1)
            .map(|train| measured[train].steady())
            .collect();
        if let Some(window) = &window
            && movement(window).is_none()
            && (lateness(window, 3) - lateness(window, 1)).abs() <= STEADY_SPAN
        {
            #[cfg(test)]
            if let Some(at) = &self.at {
                at(Phase::Analyzing);
            }
            return Some(Ok(self.calibrated(window[2], window[3], window[4])));
        }
        (measured.len() == MOST_TRAINS).then(|| Err(self.unsettled(&measured, output, now)))
    }

    /// Claude: why the latest trains, around output train `output`, show the
    /// calibration not settled: the latest train whose leg moved while it
    /// played, or else how the window's trains drifted, or else how fast the
    /// output moved against the reference between its latest two trains.
    fn unsettled(&self, measured: &[&Outcome], output: usize, now: u64) -> CalibrationFailure {
        let ticks = host_time::ticks_per_second();
        let moving = (output - 3..=output + 1)
            .rev()
            .find_map(|train| match measured[train] {
                Outcome::Moving { ms_per_second } => Some((
                    role_of(train),
                    Movement::AgainstSchedule {
                        ms_per_second: *ms_per_second,
                    },
                )),
                Outcome::Steady(_) => None,
            });
        let (role, movement) = moving.unwrap_or_else(|| {
            let window: Vec<&Measured> = (output - 3..=output + 1)
                .filter_map(|train| measured[train].steady())
                .collect();
            movement(&window).unwrap_or_else(|| {
                let seconds = (window[3].at - window[1].at) / ticks;
                let moved = lateness(&window, 3) - lateness(&window, 1);
                (
                    Role::Output,
                    Movement::BetweenTrains {
                        ms_per_second: moved / seconds * 1e3,
                    },
                )
            })
        });
        CalibrationFailure::Unsettled {
            role,
            movement,
            after_seconds: self
                .leg(role)
                .running_since
                .map_or(0.0, |since| now.saturating_sub(since) as f64 / ticks),
        }
    }

    /// Claude: the offset from output train `output` and the reference trains
    /// before and after it.
    fn calibrated(&self, first: &Measured, output: &Measured, second: &Measured) -> Calibrated {
        let along = (output.at - first.at) / (second.at - first.at);
        let reference = first.residual + (second.residual - first.residual) * along;
        let offset = f64::from(self.leg(Role::Output).latency_offset_ms)
            + (output.residual - reference) * 1000.0;
        Calibrated {
            output: Residual {
                late_ms: output.residual * 1000.0,
                spread_ms: output.spread * 1000.0,
            },
            reference: Residual {
                late_ms: reference * 1000.0,
                spread_ms: first.spread.max(second.spread) * 1000.0,
            },
            latency_offset_ms: offset.round() as i32,
        }
    }

    /// Claude: whether the router, or a program endpoint the job uses, went
    /// away.
    fn removed(&self) -> bool {
        !self.alive.load(Ordering::Acquire)
            || self.legs.iter().any(Leg::removed)
            || matches!(&self.feed, Feed::Program(input) if input.ended())
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
                Feed::Input {
                    source: Some(input),
                    ..
                } => input.remove_sink(listening.sink as usize),
                Feed::Input { source: None, .. } => {}
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

fn detectors(sample_rate: f64) -> [Detector; 2] {
    [Role::Reference, Role::Output].map(|role| Detector::new(sample_rate, role))
}

fn index(role: Role) -> usize {
    match role {
        Role::Reference => 0,
        Role::Output => 1,
    }
}

/// Claude: how a five-train window, reference, output, reference, output,
/// reference, drifted more than `STEADY_DRIFT` within its trains, the latest
/// first: an output train against the reference trains around it, or the
/// reference trains against one another. A recording whose dates drift
/// steadily drifts every train alike, which cancels.
fn movement(window: &[&Measured]) -> Option<(Role, Movement)> {
    for output in [3, 1] {
        let reference = (window[output - 1].drift + window[output + 1].drift) / 2.0;
        let drift = window[output].drift - reference;
        if drift.abs() > STEADY_DRIFT {
            return Some((
                Role::Output,
                Movement::AgainstReference {
                    ms_per_second: drift * 1e3,
                },
            ));
        }
    }
    let references = [window[0].drift, window[2].drift, window[4].drift];
    let lowest = references.iter().copied().fold(f64::INFINITY, f64::min);
    let highest = references.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (highest - lowest > STEADY_DRIFT).then_some((
        Role::Reference,
        Movement::ReferenceDrifts {
            apart_ms_per_second: (highest - lowest) * 1e3,
        },
    ))
}

/// Claude: how much later than the reference output train `train` of a
/// five-train window, reference, output, reference, output, reference, sounded.
fn lateness(window: &[&Measured], train: usize) -> f64 {
    let (first, output, second) = (window[train - 1], window[train], window[train + 1]);
    let along = (output.at - first.at) / (second.at - first.at);
    output.residual - (first.residual + (second.residual - first.residual) * along)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn train(at: f64, drift_ms_per_second: f64) -> Measured {
        Measured {
            residual: 0.0,
            spread: 0.0,
            at,
            drift: drift_ms_per_second / 1e3,
        }
    }

    /// Claude: reference trains drifting 0, 0.03 and 0 ms a second, around
    /// output trains that drift 0, fail as reference trains whose drifts lie
    /// 0.03 ms a second apart: not as 0, which the first and last alone give.
    #[test]
    fn reference_trains_drifting_apart_report_how_far_apart() {
        let window = [
            train(0.0, 0.0),
            train(5.3, 0.0),
            train(10.6, 0.03),
            train(15.9, 0.0),
            train(21.2, 0.0),
        ];
        let window: Vec<&Measured> = window.iter().collect();
        let Some((
            Role::Reference,
            Movement::ReferenceDrifts {
                apart_ms_per_second,
            },
        )) = movement(&window)
        else {
            panic!("{:?}", movement(&window))
        };
        assert!(
            (apart_ms_per_second - 0.03).abs() < 1e-9,
            "{apart_ms_per_second}"
        );
        let said = CalibrationFailure::Unsettled {
            role: Role::Reference,
            movement: Movement::ReferenceDrifts {
                apart_ms_per_second,
            },
            after_seconds: 30.0,
        }
        .to_string();
        assert!(
            said.contains("the reference's timing kept moving")
                && said.contains("at rates 0.030 ms a second apart"),
            "{said}"
        );
    }

    #[test]
    fn an_output_drifting_against_the_reference_trains_around_it_reports_that() {
        let window = [
            train(0.0, 0.01),
            train(5.3, 0.01),
            train(10.6, 0.01),
            train(15.9, 0.05),
            train(21.2, 0.01),
        ];
        let window: Vec<&Measured> = window.iter().collect();
        let Some((Role::Output, Movement::AgainstReference { ms_per_second })) = movement(&window)
        else {
            panic!("{:?}", movement(&window))
        };
        assert!((ms_per_second - 0.04).abs() < 1e-9, "{ms_per_second}");
    }
}
