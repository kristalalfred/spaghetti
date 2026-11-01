//! Claude: calibrations through the engine's real paths, program outputs as
//! destinations and a program input as the microphone, in time of the
//! tests' own. A simulated room sounds what each output renders some time
//! after the heard time it rendered for, carries it to the microphone, and
//! dates what the microphone captures with a bias of its own. Every expected
//! value comes from what the room was told.

use std::cell::Cell;
use std::f64::consts::PI;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::job::{Phase, RECORDING_LAG, Step};
use super::probe::{MIN_RECORDING_RATE, TRAIN_SECONDS};
use super::*;
use crate::program_io::{Frames, ProgramInput, ProgramOutput};
use crate::route_output::Output;
use crate::router::Router;
use crate::{HostTime, host_time};

const RENDER: usize = 240;
const STEP: f64 = 0.005;
/// Claude: the microphone delivers what it captured this long after.
const LAG: f64 = 0.1;
const TAPS: isize = 16;

/// Claude: a reproducible stream of normal deviates.
struct Noise(u64);

impl Noise {
    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    fn normal(&mut self) -> f64 {
        let (u, v) = (self.uniform().max(1e-300), self.uniform());
        (-2.0 * u.ln()).sqrt() * (2.0 * PI * v).cos()
    }
}

/// Claude: a program output in the room: how much later than its heard times
/// it really sounds, and the paths, a travel time and a gain each, its sound
/// takes to the microphone.
struct Speaker {
    output: ProgramOutput,
    rate: f64,
    latency: f64,
    lateness: f64,
    paths: Vec<(f64, f64)>,
    /// Claude: its left channel as rendered, heard from time 0 on.
    rendered: Vec<f32>,
    /// Claude: which burst of sound each rendered sample belongs to, counting
    /// from 0, or -1 between bursts.
    bursts: Vec<i32>,
    count: i32,
    /// Claude: bursts the microphone does not hear.
    muted: Vec<i32>,
    /// Claude: from this time on, renders are dated this much later.
    skip: Option<(f64, f64)>,
    moving: Option<Moving>,
}

impl Speaker {
    fn heard(&self, frame: usize) -> f64 {
        let at = frame as f64 / self.rate;
        match self.skip {
            Some((from, by)) if at >= from => at + by,
            _ => at,
        }
    }

    /// Claude: renders everything heard before `until`.
    fn render(&mut self, origin: u64, until: f64) {
        let ticks = host_time::ticks_per_second();
        while self.heard(self.rendered.len()) < until {
            let heard = self.heard(self.rendered.len());
            let (mut left, mut right) = ([0.0f32; RENDER], [0.0f32; RENDER]);
            let at = HostTime::from_ticks(origin + (heard * ticks) as u64);
            self.output.render([&mut left, &mut right], at).unwrap();
            for sample in left {
                let sounding = sample != 0.0;
                let previous = self.bursts.last().copied().unwrap_or(-1);
                let burst = match (sounding, previous) {
                    (false, _) => -1,
                    (true, -1) => {
                        let quiet_since =
                            self.bursts.iter().rev().take_while(|b| **b == -1).count();
                        if quiet_since > 100 || self.count == 0 {
                            self.count += 1;
                        }
                        self.count - 1
                    }
                    (true, burst) => burst,
                };
                self.bursts.push(burst);
                self.rendered.push(sample);
            }
        }
    }

    /// Claude: how much later than its heard times the speaker sounds at `t`.
    fn lateness_at(&self, t: f64) -> f64 {
        self.lateness
            + match self.moving {
                Some(Moving::Settling { extra, tau }) => extra * (-t / tau).exp(),
                Some(Moving::Drifting { per_second }) => per_second * t,
                Some(Moving::Swaying {
                    amplitude,
                    period,
                    rising,
                }) => amplitude * (2.0 * PI * (t - rising) / period).sin(),
                Some(Moving::Stepping { first, every, by }) if t >= first => {
                    by * (((t - first) / every).floor() + 1.0)
                }
                Some(Moving::Stepping { .. }) | None => 0.0,
            }
    }

    /// Claude: the sound at `t` seconds, by band-limited interpolation of what
    /// was rendered, as heard `lateness` after its heard time.
    fn sound(&self, t: f64) -> f64 {
        let position = (t - self.lateness_at(t)) * self.rate;
        let base = position.floor() as isize;
        let mut sum = 0.0;
        for tap in (base - TAPS + 1)..=(base + TAPS) {
            let Some(&sample) = usize::try_from(tap).ok().and_then(|i| self.rendered.get(i)) else {
                continue;
            };
            if sample == 0.0 || self.muted.contains(&self.bursts[tap as usize]) {
                continue;
            }
            let x = position - tap as f64;
            let window = 0.5 + 0.5 * (PI * x / TAPS as f64).cos();
            let sinc = if x.abs() < 1e-12 {
                1.0
            } else {
                (PI * x).sin() / (PI * x)
            };
            sum += f64::from(sample) * sinc * window;
        }
        sum
    }

    fn sounding_bursts(&self) -> i32 {
        self.count
    }
}

/// Claude: how a speaker's lateness moves from time 0 on.
#[derive(Clone, Copy, Debug)]
enum Moving {
    /// Claude: `extra` seconds later at time 0, decaying with time constant
    /// `tau`, as a receiver settles after a late start.
    Settling { extra: f64, tau: f64 },
    /// Claude: later by `per_second` each second.
    Drifting { per_second: f64 },
    /// Claude: later by `amplitude` times the sine of `period`'s phase,
    /// rising through 0 at `rising`.
    Swaying {
        amplitude: f64,
        period: f64,
        rising: f64,
    },
    /// Claude: later by `by` at `first`, and again every `every` seconds.
    Stepping { first: f64, every: f64, by: f64 },
}

/// Claude: something the microphone does wrong once, at a time.
#[derive(Clone, Copy)]
enum Fault {
    /// Claude: `frames` frames never delivered.
    Missing {
        at: f64,
        frames: usize,
    },
    /// Claude: `frames` frames delivered twice.
    Repeated {
        at: f64,
        frames: usize,
    },
    /// Claude: `frames` samples at full scale.
    Clipped {
        at: f64,
        frames: usize,
    },
    NotANumber {
        at: f64,
    },
    /// Claude: from `from` on, every push undated.
    Undated {
        from: f64,
    },
    /// Claude: from `from` on, every push dated when `from` was.
    Frozen {
        from: f64,
    },
    /// Claude: from `from` on, every push delivered ten times over, undated.
    Flooded {
        from: f64,
    },
}

struct Microphone {
    input: ProgramInput,
    rate: f64,
    /// Claude: where its first frame falls between the speakers' frames.
    phase: f64,
    /// Claude: dates it gives are this much late, plus `drift` per second.
    bias: f64,
    drift: f64,
    noise: f64,
    deviates: Noise,
    block: usize,
    next: usize,
    faults: Vec<Fault>,
}

struct Room {
    origin: u64,
    speakers: Vec<(String, Speaker)>,
    microphone: Microphone,
}

impl Room {
    fn ticks(&self, t: f64) -> u64 {
        self.origin + (t * host_time::ticks_per_second()) as u64
    }

    fn step(&mut self, t: f64) {
        for (_, speaker) in &mut self.speakers {
            let until = t + speaker.latency;
            speaker.render(self.origin, until);
        }
        let ticks = host_time::ticks_per_second();
        let mic = &mut self.microphone;
        loop {
            let start = mic.next;
            let mut frames = mic.block;
            let at = |frame: usize| mic.phase / mic.rate + frame as f64 / mic.rate;
            if at(start + frames) > t - LAG {
                return;
            }
            let since = |from: &f64| at(start) >= *from;
            let undated = mic.faults.iter().any(|fault| match fault {
                Fault::Undated { from } => since(from),
                _ => false,
            });
            let frozen = mic.faults.iter().find_map(|fault| match fault {
                Fault::Frozen { from } if since(from) => Some(*from),
                _ => None,
            });
            let flooded = mic.faults.iter().any(|fault| match fault {
                Fault::Flooded { from } => since(from),
                _ => false,
            });
            let firing = mic.faults.iter().position(|fault| {
                let when = match fault {
                    Fault::Missing { at, .. }
                    | Fault::Repeated { at, .. }
                    | Fault::Clipped { at, .. }
                    | Fault::NotANumber { at } => *at,
                    Fault::Undated { .. } | Fault::Frozen { .. } | Fault::Flooded { .. } => {
                        return false;
                    }
                };
                (at(start)..at(start + frames)).contains(&when)
            });
            let fault = firing.map(|index| mic.faults.remove(index));
            if let Some(
                Fault::Missing {
                    frames: missing, ..
                }
                | Fault::Repeated {
                    frames: missing, ..
                },
            ) = fault
            {
                frames = missing;
            }
            let mut samples: Vec<f32> = (start..start + frames)
                .map(|frame| {
                    let t = at(frame);
                    let heard: f64 = self
                        .speakers
                        .iter()
                        .flat_map(|(_, speaker)| {
                            speaker
                                .paths
                                .iter()
                                .map(move |(travel, gain)| gain * speaker.sound(t - travel))
                        })
                        .sum();
                    (heard + mic.noise * mic.deviates.normal()) as f32
                })
                .collect();
            match fault {
                Some(Fault::Clipped {
                    frames: clipped, ..
                }) => {
                    samples[..clipped].fill(1.0);
                }
                Some(Fault::NotANumber { .. }) => samples[0] = f32::NAN,
                _ => {}
            }
            let date = |frame: usize| {
                let t = frozen.unwrap_or(at(frame));
                HostTime::from_ticks(self.origin + ((t + mic.bias + mic.drift * t) * ticks) as u64)
            };
            match fault {
                _ if flooded => {
                    for _ in 0..10 {
                        mic.input.push_undated(Frames::mono(&samples).unwrap());
                    }
                }
                _ if undated => mic.input.push_undated(Frames::mono(&samples).unwrap()),
                Some(Fault::Missing { .. }) => {}
                Some(Fault::Repeated { .. }) => {
                    mic.input.push(Frames::mono(&samples).unwrap(), date(start));
                    mic.input.push(Frames::mono(&samples).unwrap(), date(start));
                }
                _ => mic.input.push(Frames::mono(&samples).unwrap(), date(start)),
            }
            mic.next = start + frames;
        }
    }
}

/// Claude: how a test sets up a speaker.
#[derive(Clone)]
struct Place {
    rate: f64,
    latency: f64,
    lateness: f64,
    paths: Vec<(f64, f64)>,
    delay_ms: u32,
    latency_offset_ms: Option<i32>,
    muted: Vec<i32>,
    skip: Option<(f64, f64)>,
    moving: Option<Moving>,
}

impl Place {
    fn at(lateness_ms: f64, travel_ms: f64) -> Self {
        Self {
            rate: 48_000.0,
            latency: 0.01,
            lateness: lateness_ms / 1000.0,
            paths: vec![(travel_ms / 1000.0, 0.3)],
            delay_ms: 300,
            latency_offset_ms: None,
            muted: Vec::new(),
            skip: None,
            moving: None,
        }
    }

    /// Claude: what a calibration of this speaker against `reference`
    /// recommends, from what the room was told.
    fn expected_offset_ms(&self, reference: &Place) -> f64 {
        let direct = |place: &Place| place.lateness + place.paths[0].0;
        (direct(self) - direct(reference)) * 1000.0
            + f64::from(reference.latency_offset_ms.unwrap_or(0))
    }

    /// Claude: how late a train of this speaker is heard against its
    /// schedule, before the microphone's bias.
    fn residual_ms(&self) -> f64 {
        (self.lateness + self.paths[0].0) * 1000.0 - f64::from(self.latency_offset_ms.unwrap_or(0))
    }
}

#[derive(Clone)]
struct Setup {
    output: Place,
    reference: Place,
    mic_rate: f64,
    phase: f64,
    bias_ms: f64,
    drift: f64,
    noise: f64,
    faults: Vec<Fault>,
    interrupted: Option<(Phase, Interruption)>,
    cancelled_at: Option<f64>,
}

/// Claude: what happens to a calibration as it reaches a phase.
#[derive(Debug, Clone, Copy)]
enum Interruption {
    Cancelled,
    Removed(&'static str),
    RouterDropped,
}

impl Setup {
    fn new(output: Place, reference: Place) -> Self {
        Self {
            output,
            reference,
            mic_rate: 48_000.0,
            phase: 0.37,
            bias_ms: 0.0,
            drift: 0.0,
            noise: 1e-4,
            faults: Vec::new(),
            interrupted: None,
            cancelled_at: None,
        }
    }
}

struct Run {
    result: Result<Calibrated, CalibrationFailure>,
    room: Room,
    /// Claude: each state the calibration reached, in turn.
    states: Vec<CalibrationState>,
}

/// Claude: runs a calibration of `setup` to its end, in the room's time.
fn run(setup: &Setup, hook: impl Fn(f64, &Job)) -> Run {
    let mut router = Router::new();
    let origin = host_time::now() + (10.0 * host_time::ticks_per_second()) as u64;
    let mut speakers = Vec::new();
    let mut destinations = Vec::new();
    for (name, place) in [("output", &setup.output), ("reference", &setup.reference)] {
        let output = router
            .add_program_output(name, place.rate, Duration::from_secs_f64(place.latency))
            .unwrap();
        speakers.push((
            name.to_string(),
            Speaker {
                output,
                rate: place.rate,
                latency: place.latency,
                lateness: place.lateness,
                paths: place.paths.clone(),
                rendered: Vec::new(),
                bursts: Vec::new(),
                count: 0,
                muted: place.muted.clone(),
                skip: place.skip,
                moving: place.moving,
            },
        ));
        destinations.push(Destination::Program {
            output: name.into(),
            delay_ms: Some(place.delay_ms),
            latency_offset_ms: place.latency_offset_ms,
        });
    }
    let input = router
        .add_program_input("microphone", setup.mic_rate)
        .unwrap();
    let mut room = Room {
        origin,
        speakers,
        microphone: Microphone {
            input,
            rate: setup.mic_rate,
            phase: setup.phase,
            bias: setup.bias_ms / 1000.0,
            drift: setup.drift,
            noise: setup.noise,
            deviates: Noise(0x2545_f491_4f6c_dd1d),
            block: (setup.mic_rate / 100.0) as usize,
            next: 0,
            faults: setup.faults.clone(),
        },
    };
    let reference = destinations.pop().unwrap();
    let output = destinations.pop().unwrap();
    let mut job = router
        .calibration_job(Calibration {
            output,
            reference,
            recording: Recording::Program {
                input: "microphone".into(),
            },
        })
        .unwrap();
    let router = Arc::new(Mutex::new(Some(router)));
    let cancelled = Arc::new(AtomicBool::new(false));
    if let Some((phase, interruption)) = setup.interrupted {
        let (router, cancelled) = (Arc::clone(&router), Arc::clone(&cancelled));
        job.at = Some(Box::new(move |now| {
            if now != phase {
                return;
            }
            let mut router = router.lock().unwrap();
            match interruption {
                Interruption::Cancelled => cancelled.store(true, Ordering::Release),
                Interruption::Removed("microphone") => router
                    .as_mut()
                    .unwrap()
                    .remove_program_input("microphone")
                    .unwrap(),
                Interruption::Removed(output) => router
                    .as_mut()
                    .unwrap()
                    .remove_program_output(output)
                    .unwrap(),
                Interruption::RouterDropped => drop(router.take()),
            }
        }));
    }
    let mut states = vec![job.state()];
    let result = (|| {
        job.prepare(&|| false)?;
        job.open(room.ticks(0.0), &|| false)?;
        let mut t = 0.0;
        loop {
            t += STEP;
            if setup.cancelled_at.is_some_and(|at| t >= at) {
                cancelled.store(true, Ordering::Release);
            }
            if cancelled.load(Ordering::Acquire) {
                return Err(CalibrationFailure::Cancelled);
            }
            hook(t, &job);
            match job.tick(room.ticks(t)) {
                Step::Done(result) => return result,
                Step::Running(state) => {
                    if states.last() != Some(&state) {
                        states.push(state);
                    }
                }
            }
            room.step(t);
            assert!(t < 90.0, "the calibration ran for a minute and a half");
        }
    })();
    let result = job.conclude(result, &|| cancelled.load(Ordering::Acquire));
    Run {
        result,
        room,
        states,
    }
}

fn calibrated(setup: &Setup) -> Calibrated {
    run(setup, |_, _| {})
        .result
        .unwrap_or_else(|failure| panic!("{failure}"))
}

/// Claude: the calibration recommends what the room says, and each residual
/// is what the room delays its trains by, the microphone's bias included.
fn assert_measures(setup: &Setup, calibrated: &Calibrated) {
    let expected = setup.output.expected_offset_ms(&setup.reference);
    assert!(
        (expected - expected.round()).abs() < 0.4,
        "{expected} ms rounds too close to call"
    );
    assert_eq!(
        calibrated.latency_offset_ms,
        expected.round() as i32,
        "{calibrated:?}"
    );
    let late = calibrated.output.late_ms - calibrated.reference.late_ms;
    let wanted = setup.output.residual_ms() - setup.reference.residual_ms();
    assert!(
        (late - wanted).abs() < 0.05,
        "{late} ms against {wanted} ms: {calibrated:?}"
    );
    let drifted = setup.drift.abs() * super::probe::layout(Role::Reference)[4] * 1000.0;
    for residual in [calibrated.output, calibrated.reference] {
        assert!(residual.spread_ms < 0.05 + drifted, "{calibrated:?}");
    }
}

#[test]
fn a_destinations_lateness_and_travel_are_recovered() {
    let setup = Setup::new(Place::at(37.3, 2.17), Place::at(4.11, 1.03));
    let calibrated = calibrated(&setup);
    assert_measures(&setup, &calibrated);
    assert!((calibrated.output.late_ms - setup.output.residual_ms()).abs() < 0.05);
    assert!((calibrated.reference.late_ms - setup.reference.residual_ms()).abs() < 0.05);
}

/// Claude: in the room as `Setup::new` places it, with 300 ms delays, the
/// output's second probe has sounded and the next is heard 521 ms later.
const BETWEEN_OUTPUT_PROBES: f64 = 9.0;
/// Claude: the output's fourth probe is scheduled at 10.51 s and heard 11 ms later.
const IN_THE_OUTPUTS_FOURTH_PROBE: f64 = 10.6;

fn failure(setup: &Setup) -> CalibrationFailure {
    run(setup, |_, _| {}).result.unwrap_err()
}

fn swapped(setup: &Setup) -> Setup {
    Setup {
        output: setup.reference.clone(),
        reference: setup.output.clone(),
        ..setup.clone()
    }
}

#[test]
fn the_reference_cancels_the_microphones_bias_constant_or_drifting() {
    let room = Setup::new(Place::at(37.3, 2.17), Place::at(4.11, 1.03));
    let unbiased = calibrated(&room);
    for (bias_ms, drift) in [(23.0, 0.0), (-15.0, 0.0), (0.0, 2e-4), (0.0, -2e-4)] {
        for setup in [room.clone(), swapped(&room)] {
            let biased = Setup {
                bias_ms,
                drift,
                ..setup.clone()
            };
            let calibrated = calibrated(&biased);
            assert_measures(&biased, &calibrated);
            if drift == 0.0 && setup.output.lateness == room.output.lateness {
                let moved = calibrated.output.late_ms - unbiased.output.late_ms;
                assert!(
                    (moved - bias_ms).abs() < 0.05,
                    "{moved} ms for {bias_ms} ms"
                );
            }
        }
    }
}

#[test]
fn a_direct_path_under_a_stronger_echo_is_the_arrival() {
    for echo in [(0.102, 0.316), (0.009, 0.5)] {
        let mut output = Place::at(12.5, 2.0);
        output.paths = vec![(0.002, 0.1), echo];
        let setup = Setup::new(output, Place::at(3.2, 1.0));
        assert_measures(&setup, &calibrated(&setup));
    }
}

/// Claude: rooms after what a built-in microphone heard: built-in speakers
/// that ring ahead of their arrival, 31 and 32 dB under it, and a soundbar
/// whose drivers arrive near-equal over 2 ms, sounding 19 dB under 2.6 ms
/// ahead, then reflected 14 and 20 ms later, 3 and 6 dB under.
#[test]
fn speakers_ringing_ahead_or_arriving_in_clusters_measure_at_their_first_arrival() {
    let under = |db: f64| 10f64.powf(-db / 20.0);
    let mut reference = Place::at(0.35, 4.0);
    reference.paths = vec![
        (0.004, 0.3),
        (0.004 - 0.00175, 0.3 * under(32.0)),
        (0.004 - 0.00277, 0.3 * under(31.0)),
        (0.010, 0.3 * under(10.0)),
    ];
    let mut output = Place::at(-43.4, 3.0);
    output.paths = vec![
        (0.003, 0.25),
        (0.0035, 0.25 * under(0.6)),
        (0.00425, 0.25 * under(0.4)),
        (0.00475, 0.26),
        (0.0004, 0.26 * under(19.0)),
        (0.017, 0.26 * under(3.0)),
        (0.023, 0.26 * under(6.0)),
    ];
    let setup = Setup::new(output, reference);
    assert_measures(&setup, &calibrated(&setup));
}

#[test]
fn each_leg_plays_only_its_own_trains_on_its_own_delay() {
    let mut output = Place::at(-6.1, 1.5);
    output.delay_ms = 800;
    let mut reference = Place::at(8.8, 0.9);
    reference.delay_ms = 250;
    reference.latency_offset_ms = Some(7);
    let setup = Setup::new(output, reference);
    let run = run(&setup, |_, _| {});
    assert_measures(&setup, &run.result.unwrap());
    let bursts: Vec<i32> = run
        .room
        .speakers
        .iter()
        .map(|(_, speaker)| speaker.sounding_bursts())
        .collect();
    assert_eq!(
        bursts,
        [10, 15],
        "probes the output and the reference played: two trains and three"
    );
}

#[test]
fn a_negative_correction_against_an_offset_reference_is_recovered() {
    let mut reference = Place::at(20.6, 3.0);
    reference.latency_offset_ms = Some(10);
    reference.delay_ms = 500;
    let setup = Setup::new(Place::at(4.0, 1.2), reference);
    let calibrated = calibrated(&setup);
    assert!(calibrated.latency_offset_ms < 0, "{calibrated:?}");
    assert_measures(&setup, &calibrated);
}

#[test]
fn calibrating_with_the_offset_applied_leaves_nothing_to_correct() {
    let setup = Setup::new(Place::at(52.7, 2.2), Place::at(4.1, 1.1));
    let first = calibrated(&setup);
    let mut corrected = setup.clone();
    corrected.output.latency_offset_ms = Some(first.latency_offset_ms);
    let second = calibrated(&corrected);
    assert_eq!(second.latency_offset_ms, first.latency_offset_ms);
    let apart = second.output.late_ms - second.reference.late_ms;
    assert!(apart.abs() < 0.5, "{apart} ms apart after correcting");
}

#[test]
fn rates_of_the_outputs_and_the_microphone_do_not_move_the_result() {
    let mut output = Place::at(23.3, 1.7);
    output.rate = 44_100.0;
    let mut reference = Place::at(2.2, 0.4);
    reference.rate = 96_000.0;
    for mic_rate in [MIN_RECORDING_RATE, 44_100.0] {
        let setup = Setup {
            mic_rate,
            ..Setup::new(output.clone(), reference.clone())
        };
        assert_measures(&setup, &calibrated(&setup));
    }
}

#[test]
fn an_output_at_the_lowest_rate_a_program_may_run_at_is_measured() {
    let mut output = Place::at(23.3, 1.7);
    output.rate = crate::MIN_PROGRAM_RATE;
    let setup = Setup::new(output, Place::at(2.2, 0.4));
    assert_measures(&setup, &calibrated(&setup));
}

#[test]
fn a_recording_too_slow_to_hold_the_probes_fails_before_anything_plays() {
    let setup = Setup {
        mic_rate: crate::MIN_PROGRAM_RATE,
        ..Setup::new(Place::at(23.3, 1.7), Place::at(2.2, 0.4))
    };
    let run = run(&setup, |_, _| {});
    let Err(CalibrationFailure::Recording(RecordingFailure::Unavailable(reason))) = run.result
    else {
        panic!("{:?}", run.result)
    };
    assert!(reason.contains("below the 16000 Hz"), "{reason}");
    assert!(
        run.room
            .speakers
            .iter()
            .all(|(_, speaker)| speaker.sounding_bursts() == 0)
    );
}

#[test]
fn quiet_or_loud_single_paths_measure_true() {
    for (gain, noise) in [(0.05, 1e-3), (0.6, 1e-5)] {
        let mut output = Place::at(14.9, 1.4);
        output.paths[0].1 = gain;
        let setup = Setup {
            noise,
            ..Setup::new(output, Place::at(3.3, 0.7))
        };
        assert_measures(&setup, &calibrated(&setup));
    }
}

#[test]
fn trains_found_with_different_probes_still_cancel_a_drifting_bias() {
    let mut output = Place::at(31.5, 2.3);
    output.muted = vec![4];
    let mut reference = Place::at(5.4, 1.1);
    reference.muted = vec![0, 7];
    let setup = Setup {
        drift: 3e-4,
        ..Setup::new(output, reference)
    };
    assert_measures(&setup, &calibrated(&setup));
}

#[test]
fn a_clipped_or_corrupt_recording_fails() {
    let mut loud = Place::at(10.0, 1.0);
    loud.paths[0].1 = 12.0;
    assert_eq!(
        failure(&Setup::new(loud, Place::at(2.0, 1.0))),
        CalibrationFailure::Recording(RecordingFailure::Clipped)
    );
    for (fault, expected) in [
        (
            Fault::Clipped {
                at: BETWEEN_OUTPUT_PROBES,
                frames: 3,
            },
            RecordingFailure::Clipped,
        ),
        (
            Fault::Clipped {
                at: IN_THE_OUTPUTS_FOURTH_PROBE,
                frames: 3,
            },
            RecordingFailure::Clipped,
        ),
        (
            Fault::NotANumber {
                at: BETWEEN_OUTPUT_PROBES,
            },
            RecordingFailure::NonFinite,
        ),
    ] {
        let setup = Setup {
            faults: vec![fault],
            ..Setup::new(Place::at(10.0, 1.0), Place::at(2.0, 1.0))
        };
        assert_eq!(failure(&setup), CalibrationFailure::Recording(expected));
    }
}

#[test]
fn frames_the_microphone_lost_or_repeated_fail_without_a_marked_discontinuity() {
    for fault in [
        Fault::Missing {
            at: BETWEEN_OUTPUT_PROBES,
            frames: 16,
        },
        Fault::Repeated {
            at: BETWEEN_OUTPUT_PROBES,
            frames: 16,
        },
        Fault::Missing {
            at: BETWEEN_OUTPUT_PROBES,
            frames: 1,
        },
        Fault::Repeated {
            at: BETWEEN_OUTPUT_PROBES,
            frames: 480,
        },
    ] {
        let setup = Setup {
            faults: vec![fault],
            ..Setup::new(Place::at(10.0, 1.0), Place::at(2.0, 1.0))
        };
        assert_eq!(
            failure(&setup),
            CalibrationFailure::Recording(RecordingFailure::Gap)
        );
    }
}

/// Claude: how long a calibration of `setup` ran, the most audio it held at
/// once in seconds, when its trains' searches ended, and how it ended.
struct Held {
    ran: f64,
    most: f64,
    train_ends: Vec<f64>,
    result: Result<Calibrated, CalibrationFailure>,
}

fn held(setup: &Setup) -> Held {
    let (ran, most, ends) = (Cell::new(0.0), Cell::new(0.0f64), Cell::new(Vec::new()));
    let result = run(setup, |t, job| {
        ran.set(t);
        most.set(most.get().max(job.held_seconds()));
        ends.set(job.train_ends());
    })
    .result;
    Held {
        ran: ran.get(),
        most: most.get(),
        train_ends: ends.take(),
        result,
    }
}

#[test]
fn a_recording_that_stops_dating_or_freezes_its_dates_fails_once_a_train_is_overdue() {
    for (fault, from, failure) in [
        (Fault::Undated { from: 2.0 }, 2.0, RecordingFailure::Undated),
        (Fault::Frozen { from: 2.0 }, 2.0, RecordingFailure::Behind),
        (Fault::Undated { from: 9.0 }, 9.0, RecordingFailure::Undated),
        (Fault::Frozen { from: 9.0 }, 9.0, RecordingFailure::Behind),
    ] {
        let setup = Setup {
            faults: vec![fault],
            ..Setup::new(Place::at(10.0, 1.0), Place::at(2.0, 1.0))
        };
        let held = held(&setup);
        assert_eq!(
            held.result,
            Err(CalibrationFailure::Recording(failure.clone()))
        );
        let ends = held.train_ends.iter().find(|end| **end > from).unwrap();
        let overdue = ends + RECORDING_LAG;
        assert!(
            (overdue..overdue + 0.02).contains(&held.ran),
            "{failure:?} from {from} s: {} s, a train ending at {ends} s",
            held.ran
        );
        assert!(held.most < held.ran, "{failure:?}: {} s held", held.most);
    }
}

#[test]
fn a_recording_flooding_undated_audio_fails_before_it_holds_much() {
    let setup = Setup {
        faults: vec![Fault::Flooded { from: 2.0 }],
        ..Setup::new(Place::at(10.0, 1.0), Place::at(2.0, 1.0))
    };
    let held = held(&setup);
    assert_eq!(
        held.result,
        Err(CalibrationFailure::Recording(RecordingFailure::Undated))
    );
    assert!(held.ran < 2.5, "{} s", held.ran);
    assert!(
        held.most < 2.0 + 2.0 * RECORDING_LAG,
        "{} s held",
        held.most
    );
}

/// Claude: none of these ever gives a calibration that would otherwise
/// succeed `Done(Ok)` once it happens as the calibration reaches `phase`.
fn interrupted_while(phase: Phase) {
    let room = Setup::new(Place::at(10.0, 1.0), Place::at(2.0, 1.0));
    for (interruption, failure) in [
        (Interruption::Cancelled, CalibrationFailure::Cancelled),
        (
            Interruption::Removed("microphone"),
            CalibrationFailure::EndpointRemoved,
        ),
        (
            Interruption::Removed("output"),
            CalibrationFailure::EndpointRemoved,
        ),
        (
            Interruption::Removed("reference"),
            CalibrationFailure::EndpointRemoved,
        ),
        (
            Interruption::RouterDropped,
            CalibrationFailure::EndpointRemoved,
        ),
    ] {
        let setup = Setup {
            interrupted: Some((phase, interruption)),
            ..room.clone()
        };
        assert_eq!(
            run(&setup, |_, _| {}).result,
            Err(failure),
            "{interruption:?} while {phase:?}"
        );
    }
}

#[test]
fn a_calibration_cancelled_or_left_without_an_endpoint_as_it_analyzes_fails() {
    interrupted_while(Phase::Analyzing);
}

#[test]
fn a_calibration_cancelled_or_left_without_an_endpoint_as_it_releases_fails() {
    interrupted_while(Phase::Releasing);
}

/// Claude: a calibration of an output whose timing moves as `moving`.
fn moving(moving: Moving) -> Held {
    let mut output = Place::at(23.3, 1.7);
    output.moving = Some(moving);
    held(&Setup::new(output, Place::at(2.2, 0.4)))
}

/// Claude: 0.3 ms a second moves an output only 0.78 ms within a train, which
/// one train's agreement allows, but its drift within the trains, and its
/// trains against the reference, show it moving until the calibration stops
/// waiting, after its fifth output train.
#[test]
fn an_output_drifting_slowly_is_not_calibrated_and_says_it_kept_moving() {
    let held = moving(Moving::Drifting { per_second: 0.0003 });
    let Err(CalibrationFailure::Unsettled {
        role: Role::Output,
        movement: Movement::AgainstReference { ms_per_second },
        after_seconds,
    }) = held.result
    else {
        panic!("{:?}", held.result)
    };
    assert!((ms_per_second - 0.3).abs() < 0.02, "{ms_per_second}");
    assert_eq!(held.train_ends.len(), 11);
    assert!((65.0..72.0).contains(&after_seconds), "{after_seconds}");
    let said = held.result.unwrap_err().to_string();
    assert!(said.contains("the output's timing kept moving"), "{said}");
}

/// Claude: an output 30 ms late at first and settling with a time constant
/// of 2 s still moves through the first output train; the calibration waits
/// for two settled ones and measures where it settled.
#[test]
fn an_output_that_settles_within_the_bound_is_measured_where_it_settled() {
    let mut output = Place::at(23.3, 1.7);
    output.moving = Some(Moving::Settling {
        extra: 0.03,
        tau: 2.0,
    });
    let setup = Setup::new(output, Place::at(2.2, 0.4));
    let held = held(&setup);
    let calibrated = held.result.unwrap_or_else(|failure| panic!("{failure}"));
    assert_measures(&setup, &calibrated);
    let seventh = held.train_ends[6];
    assert!(
        (seventh..seventh + 0.5).contains(&held.ran),
        "done at {} s, the seventh train recorded through at {seventh} s",
        held.ran
    );
}

/// Claude: a receiver that starts 115 ms late and settles with a time
/// constant of a minute, as one was measured to, keeps moving through every
/// train the calibration may play: it fails, saying so, rather than measure
/// where the receiver happened to be.
#[test]
fn an_output_settling_as_slowly_as_a_receiver_measured_is_not_calibrated() {
    let held = moving(Moving::Settling {
        extra: 0.115,
        tau: 60.0,
    });
    let Err(CalibrationFailure::Unsettled {
        role: Role::Output,
        movement: Movement::AgainstSchedule { ms_per_second },
        ..
    }) = held.result
    else {
        panic!("{:?}", held.result)
    };
    assert!((-2.0..-0.5).contains(&ms_per_second), "{ms_per_second}");
    assert_eq!(held.train_ends.len(), 11);
    assert!(held.ran < 72.0, "{} s", held.ran);
}

/// Claude: in the room as `Setup::new` places it, with 300 ms delays, a
/// train sounds every 6.2 s from 1.31 s, each for 3.7 s.
const TRAINS_FROM: f64 = 1.31;
const TRAINS_EVERY: f64 = 6.2;

/// Claude: an output swaying with the period its trains come in, rising
/// through its mean in the middle of each, sounds as late in every output
/// train but drifts within each, 0.18 ms a second: only the drift within the
/// trains shows it moving.
#[test]
fn an_output_that_sways_with_its_trains_drifts_within_them_and_is_not_calibrated() {
    let held = moving(Moving::Swaying {
        amplitude: 0.0003,
        period: 2.0 * TRAINS_EVERY,
        rising: TRAINS_FROM + TRAINS_EVERY + TRAIN_SECONDS / 2.0,
    });
    let Err(CalibrationFailure::Unsettled {
        role: Role::Output,
        movement: Movement::AgainstReference { .. },
        ..
    }) = held.result
    else {
        panic!("{:?}", held.result)
    };
}

/// Claude: an output that steps 0.5 ms later between every two trains never
/// drifts within one: only its trains against the reference, 12.4 s apart,
/// show it moving, at the 0.081 ms a second its steps average.
#[test]
fn an_output_that_steps_between_its_trains_is_not_calibrated() {
    let held = moving(Moving::Stepping {
        first: TRAINS_FROM + TRAIN_SECONDS + (TRAINS_EVERY - TRAIN_SECONDS) / 2.0,
        every: TRAINS_EVERY,
        by: 0.0005,
    });
    let Err(CalibrationFailure::Unsettled {
        role: Role::Output,
        movement: Movement::BetweenTrains { ms_per_second },
        ..
    }) = held.result
    else {
        panic!("{:?}", held.result)
    };
    assert!(
        (ms_per_second - 0.5 / TRAINS_EVERY).abs() < 0.005,
        "{ms_per_second}"
    );
}

/// Claude: `reference` heard again `under_db` under the output's path,
/// `TRAINS_EVERY` and 28 ms later, as a room or a loopback might replay it:
/// every output train's window holds a copy of the reference train before
/// it, standing still about 30 ms after the output's schedule.
fn leaking(mut reference: Place, under_db: f64) -> Place {
    reference
        .paths
        .push((TRAINS_EVERY + 0.028, 0.3 * 10f64.powf(-under_db / 20.0)));
    reference
}

/// Claude: an output drifting 2.3 ms a second, with the reference leaking
/// into every output train's window 40 or 14 dB under it, is not calibrated
/// to the leak: its louder trains moved. Standing still and heard 81.7 ms
/// after its schedule, after the leak, the output is measured true: 79 ms
/// later than the reference.
#[test]
fn reference_leaking_into_the_output_trains_does_not_stand_in_for_a_moving_output() {
    for under_db in [40.0, 14.0] {
        let mut output = Place::at(23.3, 1.7);
        output.moving = Some(Moving::Drifting { per_second: 0.0023 });
        let held = held(&Setup::new(output, leaking(Place::at(2.2, 0.4), under_db)));
        let Err(CalibrationFailure::Unsettled {
            role: Role::Output,
            movement: Movement::AgainstSchedule { ms_per_second },
            ..
        }) = held.result
        else {
            panic!("{under_db} dB under: {:?}", held.result)
        };
        assert!(
            (ms_per_second - 2.3).abs() < 0.05,
            "{under_db} dB under: {ms_per_second}"
        );
        let still = Setup::new(Place::at(80.0, 1.7), leaking(Place::at(2.2, 0.4), under_db));
        let calibrated = calibrated(&still);
        assert_measures(&still, &calibrated);
        assert_eq!(calibrated.latency_offset_ms, 79);
    }
}

/// Claude: an output heard 81.7 ms late along its direct path and 100, 200
/// and 300 ms after that, each 3 dB under the one before, as a room ringing
/// back and forth: the echoes of different probes cannot line up as an
/// earlier train, so the calibration finds the output 79 ms later than the
/// reference, and finds 79 ms again with it applied. With the reference
/// ringing instead, it finds -79 ms, and again with it applied.
#[test]
fn reflections_of_different_probes_do_not_line_up_as_an_earlier_train() {
    let mut ringing = Place::at(80.0, 1.7);
    ringing.paths = vec![
        (0.0017, 0.3),
        (0.1017, 0.21),
        (0.2017, 0.147),
        (0.3017, 0.1029),
    ];
    let setup = Setup::new(ringing, Place::at(2.2, 0.4));
    for (setup, offset) in [(setup.clone(), 79), (swapped(&setup), -79)] {
        let first = calibrated(&setup);
        assert_measures(&setup, &first);
        assert_eq!(first.latency_offset_ms, offset, "{first:?}");
        let mut corrected = setup.clone();
        corrected.output.latency_offset_ms = Some(offset);
        let second = calibrated(&corrected);
        assert_eq!(second.latency_offset_ms, offset, "{second:?}");
    }
}

/// Claude: a calibration says which train it waits for or plays, counted
/// from 1, the reference's odd and the output's even, one after another:
/// `Calibrating::FEWEST_TRAINS` when its output settles at once, and
/// `Calibrating::MOST_TRAINS` before it fails `Unsettled` when it never does.
#[test]
fn a_calibration_counts_the_trains_it_plays() {
    let still = Setup::new(Place::at(23.3, 1.7), Place::at(2.2, 0.4));
    let mut drifting = still.clone();
    drifting.output.moving = Some(Moving::Drifting { per_second: 0.0003 });
    for (setup, played) in [
        (still, Calibrating::FEWEST_TRAINS),
        (drifting, Calibrating::MOST_TRAINS),
    ] {
        let run = run(&setup, |_, _| {});
        let mut expected = Vec::new();
        for train in 1..=played {
            let role = if train % 2 == 1 {
                Role::Reference
            } else {
                Role::Output
            };
            if train == 1 {
                expected.push(CalibrationState::Waiting { role, train });
            }
            expected.push(CalibrationState::Probing { role, train });
        }
        if played == Calibrating::MOST_TRAINS {
            expected.push(CalibrationState::Analyzing);
        }
        assert_eq!(run.states, expected, "{:?}", run.result);
        assert_eq!(
            run.result.is_ok(),
            played == Calibrating::FEWEST_TRAINS,
            "{:?}",
            run.result
        );
    }
    assert_eq!(
        (Calibrating::FEWEST_TRAINS, Calibrating::MOST_TRAINS),
        (5, 11)
    );
}

#[test]
fn a_calibration_waiting_for_its_output_to_settle_cancels() {
    let mut output = Place::at(23.3, 1.7);
    output.moving = Some(Moving::Drifting { per_second: 0.0003 });
    let setup = Setup {
        cancelled_at: Some(30.0),
        ..Setup::new(output, Place::at(2.2, 0.4))
    };
    let held = held(&setup);
    assert_eq!(held.result, Err(CalibrationFailure::Cancelled));
    assert!((29.99..30.1).contains(&held.ran), "{} s", held.ran);
}

#[test]
fn an_output_that_strays_from_its_schedule_during_a_train_fails() {
    let mut skipping = Place::at(10.0, 1.0);
    skipping.skip = Some((BETWEEN_OUTPUT_PROBES, 0.03));
    assert_eq!(
        failure(&Setup::new(skipping, Place::at(2.0, 1.0))),
        CalibrationFailure::Glitched { role: Role::Output }
    );
    let unstamped = std::cell::Cell::new(false);
    let run = run(
        &Setup::new(Place::at(10.0, 1.0), Place::at(2.0, 1.0)),
        |t, job| {
            if t >= BETWEEN_OUTPUT_PROBES && !unstamped.replace(true) {
                let Some(Output::Program(route)) = job.legs()[1].output() else {
                    panic!("the output plays to a program output");
                };
                route.health().count_unstamped_render();
            }
        },
    );
    assert_eq!(
        run.result,
        Err(CalibrationFailure::Glitched { role: Role::Output })
    );
}

mod lifecycle {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::Instant;

    use super::*;
    use crate::spec::SourceSpec;

    const LATENCY: Duration = Duration::from_millis(10);
    const OUTPUTS: [&str; 3] = ["output", "reference", "other"];

    /// Claude: the program outputs rendered and the microphone pushed in real
    /// time on a thread of their own, the microphone hearing every output
    /// exactly when its render says it is heard, and a tone pushed into an
    /// input whose route plays to "other".
    struct Live {
        stop: Arc<AtomicBool>,
        peaks: Arc<Mutex<HashMap<&'static str, f32>>>,
        thread: Option<JoinHandle<()>>,
    }

    impl Live {
        fn start(
            outputs: Vec<(&'static str, ProgramOutput)>,
            mut microphone: ProgramInput,
            mut tone: ProgramInput,
        ) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let peaks: Arc<Mutex<HashMap<&'static str, f32>>> = Arc::default();
            let thread = std::thread::spawn({
                let (stop, peaks) = (Arc::clone(&stop), Arc::clone(&peaks));
                move || {
                    let mut outputs = outputs;
                    let ticks = host_time::ticks_per_second();
                    let frame = ticks / 48_000.0;
                    let start = host_time::now();
                    let mut next = 0usize;
                    while !stop.load(Ordering::Acquire) {
                        let now = host_time::now();
                        while (start as f64 + next as f64 * frame) < now as f64 + 0.02 * ticks {
                            let heard = start + (next as f64 * frame) as u64;
                            let mut heard_all = [0.0f32; RENDER];
                            for (name, output) in &mut outputs {
                                let (mut left, mut right) = ([0.0f32; RENDER], [0.0f32; RENDER]);
                                output
                                    .render([&mut left, &mut right], HostTime::from_ticks(heard))
                                    .unwrap();
                                let peak = left.iter().fold(0.0f32, |p, s| p.max(s.abs()));
                                let mut peaks = peaks.lock().unwrap();
                                let held = peaks.entry(*name).or_insert(0.0);
                                *held = (*held).max(peak);
                                if *name != "other" {
                                    for (all, sample) in heard_all.iter_mut().zip(left) {
                                        *all += 0.5 * sample;
                                    }
                                }
                            }
                            microphone.push(
                                Frames::mono(&heard_all).unwrap(),
                                HostTime::from_ticks(heard),
                            );
                            tone.push(
                                Frames::mono(&[0.25; RENDER]).unwrap(),
                                HostTime::from_ticks(heard),
                            );
                            next += RENDER;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
            });
            Self {
                stop,
                peaks,
                thread: Some(thread),
            }
        }

        /// Claude: the loudest each output played over the next `seconds`.
        fn loudest(&self, seconds: f64) -> HashMap<&'static str, f32> {
            self.peaks.lock().unwrap().clear();
            std::thread::sleep(Duration::from_secs_f64(seconds));
            self.peaks.lock().unwrap().clone()
        }
    }

    impl Drop for Live {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }

    /// Claude: a router with the three program outputs, the microphone and a
    /// tone input routed to "other".
    fn studio() -> (
        Router,
        Vec<(&'static str, ProgramOutput)>,
        ProgramInput,
        ProgramInput,
    ) {
        let mut router = Router::new();
        let outputs = OUTPUTS
            .iter()
            .map(|name| {
                (
                    *name,
                    router.add_program_output(name, 48_000.0, LATENCY).unwrap(),
                )
            })
            .collect();
        let microphone = router.add_program_input("microphone", 48_000.0).unwrap();
        let tone = router.add_program_input("tone", 48_000.0).unwrap();
        let source = router
            .create_source(
                SourceSpec::Program {
                    input: "tone".into(),
                },
                None,
            )
            .unwrap();
        router.create_route(source, to("other", 200), 1.0).unwrap();
        (router, outputs, microphone, tone)
    }

    fn to(output: &str, delay_ms: u32) -> Destination {
        Destination::Program {
            output: output.into(),
            delay_ms: Some(delay_ms),
            latency_offset_ms: None,
        }
    }

    fn calibration() -> Calibration {
        Calibration {
            output: to("output", 200),
            reference: to("reference", 200),
            recording: Recording::Program {
                input: "microphone".into(),
            },
        }
    }

    /// Claude: waits up to `limit` for the calibration to be done.
    fn done(calibrating: &Calibrating, limit: Duration) -> Result<Calibrated, CalibrationFailure> {
        let deadline = Instant::now() + limit;
        loop {
            if let CalibrationState::Done(result) = calibrating.state() {
                return result;
            }
            assert!(Instant::now() < deadline, "still {:?}", calibrating.state());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for(calibrating: &Calibrating, wanted: impl Fn(&CalibrationState) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !wanted(&calibrating.state()) {
            assert!(Instant::now() < deadline, "still {:?}", calibrating.state());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_calibration_opens_nothing_before_its_thread_does_and_cancels_while_it_stalls() {
        let (mut router, _outputs, _microphone, _tone) = studio();
        let hold = Arc::new(AtomicBool::new(true));
        let mut job = router.calibration_job(calibration()).unwrap();
        job.hold = Some(Arc::clone(&hold));
        let started = Instant::now();
        let calibrating = Calibrating::start(job).unwrap();
        assert!(started.elapsed() < Duration::from_millis(50));
        assert_eq!(
            calibrating.state(),
            CalibrationState::Waiting {
                role: Role::Reference,
                train: 1,
            }
        );
        assert!(router.program_output_idle("output") && router.program_output_idle("reference"));
        assert!(router.program_input_idle("microphone"));
        let refused = router.create_route(1, to("output", 200), 1.0).unwrap_err();
        assert!(refused.contains("being calibrated"), "{refused}");

        let cancelled = Instant::now();
        calibrating.cancel();
        assert_eq!(router.list_routes().len(), 1);
        assert_eq!(
            done(&calibrating, Duration::from_millis(200)),
            Err(CalibrationFailure::Cancelled)
        );
        assert!(cancelled.elapsed() < Duration::from_millis(200));
        router.create_route(1, to("output", 200), 1.0).unwrap();
        drop(hold);
    }

    #[test]
    fn calibrating_looks_up_no_device_on_the_calling_thread() {
        use crate::device_lookup::{self, STALLED, UID};

        let (mut router, _outputs, _microphone, _tone) = studio();
        let stalled = Destination::Device {
            uid: UID.into(),
            delay_ms: Some(200),
            latency_offset_ms: None,
        };
        let cases = [
            (
                Calibration {
                    output: stalled,
                    ..calibration()
                },
                CalibrationFailure::Unavailable {
                    role: Role::Output,
                    reason: format!("device '{UID}' not found"),
                },
            ),
            (
                Calibration {
                    recording: Recording::Input {
                        device_uid: UID.into(),
                    },
                    ..calibration()
                },
                CalibrationFailure::Recording(RecordingFailure::Unavailable(format!(
                    "input device '{UID}' not found"
                ))),
            ),
        ];
        for (calibration, failure) in cases {
            STALLED.store(true, Ordering::Release);
            let (called, watched) = std::sync::mpsc::channel::<()>();
            let watchdog = std::thread::spawn(move || {
                if watched.recv_timeout(Duration::from_secs(2)).is_err() {
                    STALLED.store(false, Ordering::Release);
                }
            });
            let started = Instant::now();
            let calibrating = router.calibrate(calibration).unwrap();
            let returned = started.elapsed();
            let _ = called.send(());
            watchdog.join().unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            let threads = loop {
                let threads = device_lookup::take_threads();
                if !threads.is_empty() || Instant::now() > deadline {
                    break threads;
                }
                std::thread::sleep(Duration::from_millis(1));
            };
            let while_stalled = calibrating.state();
            STALLED.store(false, Ordering::Release);
            assert!(returned < Duration::from_millis(50), "{returned:?}");
            assert!(!threads.is_empty(), "the device was never looked up");
            assert!(!threads.contains(&std::thread::current().id()));
            assert_eq!(
                while_stalled,
                CalibrationState::Waiting {
                    role: Role::Reference,
                    train: 1,
                }
            );
            assert_eq!(done(&calibrating, Duration::from_secs(2)), Err(failure));
            assert!(
                router.program_output_idle("output") && router.program_output_idle("reference")
            );
            assert!(router.program_input_idle("microphone"));
        }
    }

    #[test]
    fn a_finished_or_failed_calibration_releases_everything_on_its_own() {
        let (mut router, outputs, microphone, tone) = studio();
        let calibrating = router.calibrate(calibration()).unwrap();
        let live = Live::start(outputs, microphone, tone);
        let calibrated = done(&calibrating, Duration::from_secs(40)).unwrap();
        assert!(calibrated.latency_offset_ms.abs() <= 1, "{calibrated:?}");
        assert!(calibrated.output.late_ms.abs() < 0.5 && calibrated.reference.late_ms.abs() < 0.5);
        for _ in 0..3 {
            assert_eq!(calibrating.state(), CalibrationState::Done(Ok(calibrated)));
        }
        assert!(router.program_output_idle("output") && router.program_output_idle("reference"));
        assert!(router.program_input_idle("microphone"));
        let loudest = live.loudest(0.1);
        assert_eq!((loudest["output"], loudest["reference"]), (0.0, 0.0));
        assert!(loudest["other"] > 0.2);
        drop(live);

        let (mut router, outputs, microphone, tone) = studio();
        let calibrating = router.calibrate(calibration()).unwrap();
        let live = Live::start(outputs, microphone, tone);
        wait_for(&calibrating, |state| {
            matches!(state, CalibrationState::Probing { .. })
        });
        drop(live);
        assert_eq!(
            done(&calibrating, Duration::from_secs(2)),
            Err(CalibrationFailure::Recording(RecordingFailure::Stalled))
        );
        assert!(router.program_output_idle("output") && router.program_output_idle("reference"));
        assert!(router.program_input_idle("microphone"));
        router.create_route(1, to("output", 200), 1.0).unwrap();
    }

    #[test]
    fn removing_an_endpoint_or_the_router_ends_the_calibration_and_nothing_else() {
        for probing in [false, true] {
            for removed in ["microphone", "output", "reference", "router"] {
                let (mut router, outputs, microphone, tone) = studio();
                let calibrating = router.calibrate(calibration()).unwrap();
                let live = probing.then(|| Live::start(outputs, microphone, tone));
                if probing {
                    wait_for(&calibrating, |state| {
                        matches!(
                            state,
                            CalibrationState::Probing {
                                role: Role::Reference,
                                train: 1,
                            }
                        )
                    });
                } else {
                    wait_for(&calibrating, |state| {
                        matches!(
                            state,
                            CalibrationState::Waiting {
                                role: Role::Reference,
                                train: 1,
                            }
                        )
                    });
                    std::thread::sleep(Duration::from_millis(50));
                }
                match removed {
                    "microphone" => router.remove_program_input("microphone").unwrap(),
                    "router" => drop(router),
                    output => router.remove_program_output(output).unwrap(),
                }
                let label = format!("{removed} removed while probing: {probing}");
                assert_eq!(
                    done(&calibrating, Duration::from_secs(2)),
                    Err(CalibrationFailure::EndpointRemoved),
                    "{label}"
                );
                if let Some(live) = &live {
                    let loudest = live.loudest(0.1);
                    assert_eq!(
                        (loudest["output"], loudest["reference"]),
                        (0.0, 0.0),
                        "{label}"
                    );
                    if removed != "router" {
                        assert!(loudest["other"] > 0.2, "{label}: {loudest:?}");
                    }
                }
            }
        }
    }
}
