//! Claude: calibrations through the engine's real paths, program outputs as
//! destinations and a program input as the microphone, in time of the
//! tests' own. A simulated room sounds what each output renders some time
//! after the heard time it rendered for, carries it to the microphone, and
//! dates what the microphone captures with a bias of its own. Every expected
//! value comes from what the room was told.

use std::f64::consts::PI;
use std::time::Duration;

use super::job::Step;
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

    /// Claude: the sound at `t` seconds, by band-limited interpolation of what
    /// was rendered, as heard `lateness` after its heard time.
    fn sound(&self, t: f64) -> f64 {
        let position = (t - self.lateness) * self.rate;
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
            let firing = mic.faults.iter().position(|fault| {
                let when = match fault {
                    Fault::Missing { at, .. }
                    | Fault::Repeated { at, .. }
                    | Fault::Clipped { at, .. }
                    | Fault::NotANumber { at } => *at,
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
                let t = at(frame);
                HostTime::from_ticks(self.origin + ((t + mic.bias + mic.drift * t) * ticks) as u64)
            };
            match fault {
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
        }
    }
}

struct Run {
    result: Result<Calibrated, CalibrationFailure>,
    room: Room,
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
    let result = (|| {
        job.open(room.ticks(0.0), &|| false)?;
        let mut t = 0.0;
        loop {
            t += STEP;
            hook(t, &job);
            if let Step::Done(result) = job.tick(room.ticks(t)) {
                return result;
            }
            room.step(t);
            assert!(t < 60.0, "the calibration ran for a minute");
        }
    })();
    job.release();
    Run { result, room }
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
    let drifted = setup.drift.abs() * super::probe::TRAIN[4] * 1000.0;
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
/// output's second probe has sounded and the next starts 400 ms later.
const BETWEEN_OUTPUT_PROBES: f64 = 8.15;
/// Claude: the output's fourth probe is scheduled at 8.405 s and heard 11 ms later.
const IN_THE_OUTPUTS_FOURTH_PROBE: f64 = 8.5;

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
fn an_echo_is_never_taken_for_the_direct_path() {
    for echo in [(0.102, 0.316), (0.009, 0.5)] {
        let mut output = Place::at(12.5, 2.0);
        output.paths = vec![(0.002, 0.1), echo];
        let setup = Setup::new(output, Place::at(3.2, 1.0));
        match run(&setup, |_, _| {}).result {
            Ok(calibrated) => assert_measures(&setup, &calibrated),
            Err(CalibrationFailure::Probes {
                role: Role::Output,
                failure: ProbeFailure::Ambiguous,
            }) => {}
            Err(other) => panic!("echo {echo:?}: {other}"),
        }
    }
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
        [5, 10],
        "probes the output and the reference played"
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
    for mic_rate in [16_000.0, 44_100.0] {
        let setup = Setup {
            mic_rate,
            ..Setup::new(output.clone(), reference.clone())
        };
        assert_measures(&setup, &calibrated(&setup));
    }
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
    loud.paths[0].1 = 2.5;
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
            CalibrationState::Waiting(Role::Reference)
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
            matches!(state, CalibrationState::Probing(_))
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
                        matches!(state, CalibrationState::Probing(Role::Reference))
                    });
                } else {
                    wait_for(&calibrating, |state| {
                        matches!(state, CalibrationState::Waiting(Role::Reference))
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
