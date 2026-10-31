//! Claude: audio the embedding program captures and plays itself, from its own
//! real-time callbacks. A program registers an input or an output with the
//! router under a name and keeps its real-time end; `SourceSpec::Program` and
//! `Destination::Program` then name it like any other source or destination,
//! so sources and routes come and go while the program's callback keeps
//! running. The program's clock drives the output: each callback renders
//! every route to the output, mixed, and the routes' playouts resample and
//! schedule against it as they would against a device.
//!
//! Claude: both real-time ends take host times on the clock Core Audio stamps
//! `AudioTimeStamp::mHostTime` with. An input's pushes are dated by when their
//! first frame was captured, and one push dated more than a millisecond off
//! where the previous one ended starts over: the routes drop what they still
//! hold from before it and prime again, as they do after a push the input
//! marks with `discontinuity` and after one a route had no room for.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use spaghetti_common::ipc_protocol::FlowStatus;

use crate::delivery::DeliverySpan;
use crate::flow::{FlowMeter, Link};
use crate::host_time::{self, HostTime};
use crate::mixer::{Cycles, Fader};
use crate::playout::{MAX_RENDER_FRAMES, Playout, PlayoutHealth, Schedule};
use crate::ring::{CHANNELS, Captured, Ring};
use crate::rt_slots::RtSlots;
use crate::sinks::{MAX_SINKS, Sinks, SourceTiming};
use crate::spec::Playback;

/// Claude: how many routes one program output mixes.
pub const MAX_PROGRAM_ROUTES: usize = 8;
/// Claude: the most frames one push or one render may carry.
pub const MAX_CALLBACK_FRAMES: usize = MAX_RENDER_FRAMES;

/// Claude: why a program's buffer was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferError {
    /// Claude: more than `MAX_CALLBACK_FRAMES` frames.
    TooLong { frames: usize },
    /// Claude: interleaved stereo samples that leave a frame half full.
    PartialFrame { samples: usize },
    /// Claude: two planes of different lengths.
    UnequalPlanes { left: usize, right: usize },
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong { frames } => write!(
                f,
                "{frames} frames is more than the {MAX_CALLBACK_FRAMES} a callback may carry"
            ),
            Self::PartialFrame { samples } => {
                write!(
                    f,
                    "{samples} interleaved samples do not make whole stereo frames"
                )
            }
            Self::UnequalPlanes { left, right } => {
                write!(
                    f,
                    "a left plane of {left} frames and a right plane of {right}"
                )
            }
        }
    }
}

impl std::error::Error for BufferError {}

fn checked_frames(frames: usize) -> Result<usize, BufferError> {
    if frames > MAX_CALLBACK_FRAMES {
        return Err(BufferError::TooLong { frames });
    }
    Ok(frames)
}

/// Claude: one callback's 32-bit float PCM for a program input, in whole
/// frames, at most `MAX_CALLBACK_FRAMES` of them.
#[derive(Clone, Copy, Debug)]
pub struct Frames<'a>(Layout<'a>);

#[derive(Clone, Copy, Debug)]
enum Layout<'a> {
    Mono(&'a [f32]),
    Interleaved(&'a [f32]),
    Planar(&'a [f32], &'a [f32]),
}

impl<'a> Frames<'a> {
    /// Claude: one channel, which routes play on both at its own level.
    pub fn mono(samples: &'a [f32]) -> Result<Self, BufferError> {
        checked_frames(samples.len())?;
        Ok(Self(Layout::Mono(samples)))
    }

    /// Claude: left and right interleaved.
    pub fn interleaved(samples: &'a [f32]) -> Result<Self, BufferError> {
        if !samples.len().is_multiple_of(CHANNELS) {
            return Err(BufferError::PartialFrame {
                samples: samples.len(),
            });
        }
        checked_frames(samples.len() / CHANNELS)?;
        Ok(Self(Layout::Interleaved(samples)))
    }

    /// Claude: left and right in planes of their own.
    pub fn planar(left: &'a [f32], right: &'a [f32]) -> Result<Self, BufferError> {
        if left.len() != right.len() {
            return Err(BufferError::UnequalPlanes {
                left: left.len(),
                right: right.len(),
            });
        }
        checked_frames(left.len())?;
        Ok(Self(Layout::Planar(left, right)))
    }

    pub fn len(&self) -> usize {
        match self.0 {
            Layout::Mono(samples) => samples.len(),
            Layout::Interleaved(samples) => samples.len() / CHANNELS,
            Layout::Planar(left, _) => left.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn sample(&self, frame: usize, channel: usize) -> f32 {
        match self.0 {
            Layout::Mono(samples) => samples[frame],
            Layout::Interleaved(samples) => samples[frame * CHANNELS + channel],
            Layout::Planar(left, right) => [left, right][channel][frame],
        }
    }
}

/// Claude: a program's audio rate must be a rate.
fn checked_rate(sample_rate: f64) -> Result<f64, String> {
    if sample_rate.is_finite() && sample_rate > 0.0 {
        Ok(sample_rate)
    } else {
        Err(format!("{sample_rate} Hz is not a sample rate"))
    }
}

/// Claude: what a program input shares with the sources made from it.
pub(crate) struct InputShared {
    name: String,
    sample_rate: f64,
    sinks: Sinks,
    delivery: Arc<DeliverySpan>,
    meter: FlowMeter,
}

impl InputShared {
    pub(crate) fn new(name: &str, sample_rate: f64) -> Result<Arc<Self>, String> {
        let sample_rate = checked_rate(sample_rate)?;
        host_time::ticks_per_second();
        Ok(Arc::new(Self {
            name: name.to_string(),
            sample_rate,
            sinks: Sinks::new(),
            delivery: Arc::new(DeliverySpan::new(0.0)),
            meter: FlowMeter::new(),
        }))
    }
}

/// Claude: the real-time end of a program input. The program moves it into the
/// callback that captures its audio and pushes each buffer as it arrives. It
/// keeps working after `Router::remove_program_input` and feeds nothing. Drop
/// it off the real-time thread: the last owner frees the input's state.
pub struct ProgramInput {
    shared: Arc<InputShared>,
}

impl ProgramInput {
    pub(crate) fn new(shared: Arc<InputShared>) -> Self {
        Self { shared }
    }

    /// Claude: real-time safe: no locks, no allocation. `captured` is the host
    /// time the first frame was captured, its input's latency counted back
    /// from the buffer's timestamp, which routes on a delay play it by.
    /// Consecutive pushes of unbroken audio are dated their length apart;
    /// one dated more than a millisecond off starts over. Each push is one
    /// delivery: routes keep a push's length buffered, so a program pushes
    /// each callback's buffer whole rather than splitting it. A route with no
    /// room for a push drops it and starts over with the next.
    pub fn push(&mut self, frames: Frames<'_>, captured: HostTime) {
        let count = frames.len();
        if count == 0 {
            return;
        }
        let now = host_time::now();
        self.shared.delivery.publish(count as f64);
        // Claude: `ProgramInput` is neither `Clone` nor `Sync` through `&self`,
        // and `push` takes `&mut self`, so one thread at a time visits the sinks.
        unsafe {
            self.shared.sinks.for_each(|ring| {
                ring.push_with(
                    count,
                    now,
                    Captured::Measured(captured.ticks()),
                    |frame, channel| frames.sample(frame, channel),
                );
            });
        }
        self.shared
            .meter
            .record((count * CHANNELS * size_of::<f32>()) as u64, now);
    }

    /// Claude: real-time safe. The next push starts over, as after a capture
    /// that failed or restarted, however it is dated.
    pub fn discontinuity(&mut self) {
        unsafe { self.shared.sinks.for_each(|ring| ring.discontinuity()) };
    }
}

/// Claude: a source the router made from a program input. Several sources may
/// read one input; they share its sinks.
pub(crate) struct ProgramSource {
    shared: Arc<InputShared>,
    pub label: String,
}

impl ProgramSource {
    pub(crate) fn open(shared: Arc<InputShared>) -> Self {
        Self {
            label: shared.name.clone(),
            shared,
        }
    }

    pub(crate) fn timing(&self) -> SourceTiming {
        SourceTiming {
            sample_rate: self.shared.sample_rate,
            delivery: Arc::clone(&self.shared.delivery),
        }
    }

    pub(crate) fn add_sink(&mut self, ring: Arc<Ring>) -> Result<usize, String> {
        self.shared
            .sinks
            .insert(Box::new(ring))
            .map_err(|_| format!("a program input can feed at most {MAX_SINKS} routes"))
    }

    pub(crate) fn remove_sink(&mut self, slot: usize) {
        drop(self.shared.sinks.remove(slot));
    }

    /// Claude: `Idle` without routes; with them, `Connected` until the input
    /// pushes, `Flowing` while it does and `Stalled` 3 s after it stops.
    pub(crate) fn status(&self) -> FlowStatus {
        if self.shared.sinks.is_empty() {
            return self.shared.meter.status(Link::Idle);
        }
        self.shared.meter.status(Link::Connected {
            peer: None,
            packets_lost: 0,
            packets_retransmitted: 0,
        })
    }
}

/// Claude: one route's share of a program output's render, touched only by
/// the output's real-time end once inserted.
struct RouteRender {
    ring: Arc<Ring>,
    playout: Playout,
    volume: Arc<AtomicU32>,
    fader: Fader,
    meter: Arc<FlowMeter>,
    cycles: Arc<Cycles>,
}

/// Claude: what a program output shares with the routes to it.
pub(crate) struct OutputShared {
    sample_rate: f64,
    /// Claude: the least time from a render to its first frame being heard,
    /// in seconds and in host ticks.
    latency: f64,
    latency_ticks: u64,
    routes: RtSlots<RouteRender, MAX_PROGRAM_ROUTES>,
}

impl OutputShared {
    pub(crate) fn new(sample_rate: f64, latency: Duration) -> Result<Arc<Self>, String> {
        let sample_rate = checked_rate(sample_rate)?;
        Ok(Arc::new(Self {
            sample_rate,
            latency: latency.as_secs_f64(),
            latency_ticks: host_time::from_nanos(latency.as_nanos() as u64),
            routes: RtSlots::new(),
        }))
    }
}

/// Claude: the real-time end of a program output. The program moves it into
/// the callback that plays its audio and renders each buffer from it. It
/// keeps rendering silence after `Router::remove_program_output`. Drop it off
/// the real-time thread: the last owner frees the output's state.
pub struct ProgramOutput {
    shared: Arc<OutputShared>,
    scratch: [Box<[f32]>; CHANNELS],
}

impl ProgramOutput {
    pub(crate) fn new(shared: Arc<OutputShared>) -> Self {
        Self {
            shared,
            scratch: std::array::from_fn(|_| vec![0.0; MAX_CALLBACK_FRAMES].into_boxed_slice()),
        }
    }

    /// Claude: real-time safe: no locks, no allocation. Fills both planes, at
    /// the output's rate, with every route to it mixed and clamped to
    /// [-1.0, 1.0]. `heard` is the host time the first frame will be heard,
    /// the program's own latency included, which routes on a delay schedule
    /// against. A route that runs out of audio adds silence and primes again.
    /// Planes of different lengths, or longer than `MAX_CALLBACK_FRAMES`, are
    /// refused and filled with silence, which routes report as `Silent`.
    pub fn render(
        &mut self,
        out: [&mut [f32]; CHANNELS],
        heard: HostTime,
    ) -> Result<(), BufferError> {
        let [left, right] = out;
        left.fill(0.0);
        right.fill(0.0);
        let now = host_time::now();
        let checked = if left.len() == right.len() {
            checked_frames(left.len())
        } else {
            Err(BufferError::UnequalPlanes {
                left: left.len(),
                right: right.len(),
            })
        };
        let frames = match checked {
            Ok(frames) => frames,
            Err(refused) => {
                // Claude: a refused render is still the output's latest, and
                // it played every route silence.
                unsafe {
                    self.shared.routes.for_each(|route| {
                        route.playout.refused();
                        route.cycles.tick(now);
                    });
                }
                return Err(refused);
            }
        };
        if frames == 0 {
            return Ok(());
        }
        let output = heard.ticks().saturating_sub(self.shared.latency_ticks);
        let [scratch_left, scratch_right] = &mut self.scratch;
        // Claude: `render` takes `&mut self`, so one thread at a time visits the routes.
        unsafe {
            self.shared.routes.for_each(|route| {
                let mut planes = [&mut scratch_left[..frames], &mut scratch_right[..frames]];
                route.playout.render(&route.ring, &mut planes, now, output);
                if route.playout.is_playing() {
                    route
                        .meter
                        .record((frames * CHANNELS * size_of::<f32>()) as u64, now);
                }
                let volume = f32::from_bits(route.volume.load(Ordering::Relaxed));
                route.fader.planar(&mut planes, volume);
                for (mixed, routed) in left.iter_mut().zip(planes[0].iter()) {
                    *mixed += routed;
                }
                for (mixed, routed) in right.iter_mut().zip(planes[1].iter()) {
                    *mixed += routed;
                }
                route.cycles.tick(now);
            });
        }
        for sample in left.iter_mut().chain(right.iter_mut()) {
            *sample = sample.clamp(-1.0, 1.0);
        }
        Ok(())
    }
}

/// Claude: the router's end of a route to a program output. Dropping it takes
/// the route out of the output's mix once no render is reading it.
pub(crate) struct ProgramRoute {
    output: Arc<OutputShared>,
    slot: usize,
    health: Arc<PlayoutHealth>,
    meter: Arc<FlowMeter>,
    cycles: Arc<Cycles>,
    scheduled: bool,
    reported_glitches: [u32; 5],
}

impl ProgramRoute {
    /// Claude: with a delay, each frame is heard that long after its capture.
    /// The program passes the time each render is heard, so the output's
    /// latency only bounds how long the route holds a frame before it renders.
    pub(crate) fn open(
        output: &Arc<OutputShared>,
        source: SourceTiming,
        ring: Arc<Ring>,
        volume: Arc<AtomicU32>,
        delay: Option<f64>,
    ) -> Result<Self, String> {
        let schedule = delay.map(|delay| Schedule {
            delay,
            latency: output.latency,
        });
        let playout = Playout::new(
            &source,
            output.sample_rate,
            host_time::ticks_per_second(),
            schedule,
        )?;
        tracing::info!(
            src_rate = source.sample_rate,
            dst_rate = output.sample_rate,
            delay_frames = playout.delay_frames(),
            "created resampler"
        );
        let health = playout.health();
        let meter = Arc::new(FlowMeter::new());
        let cycles = Arc::new(Cycles::new());
        let render = RouteRender {
            ring,
            playout,
            fader: Fader::new(
                f32::from_bits(volume.load(Ordering::Relaxed)),
                output.sample_rate,
            ),
            volume,
            meter: Arc::clone(&meter),
            cycles: Arc::clone(&cycles),
        };
        let slot = output
            .routes
            .insert(Box::new(render))
            .map_err(|_| format!("a program output mixes at most {MAX_PROGRAM_ROUTES} routes"))?;
        Ok(Self {
            output: Arc::clone(output),
            slot,
            health,
            meter,
            cycles,
            scheduled: schedule.is_some(),
            reported_glitches: [0; 5],
        })
    }

    /// Claude: `Connected` until the route's playout primes.
    pub(crate) fn status(&self) -> FlowStatus {
        self.meter.status(Link::Connected {
            peer: None,
            packets_lost: 0,
            packets_retransmitted: 0,
        })
    }

    pub(crate) fn playback(&self) -> Playback {
        self.health.playback(&self.cycles)
    }

    pub(crate) fn cycles(&self) -> &Cycles {
        &self.cycles
    }

    pub(crate) fn log_health(&mut self, route_id: u32, ring: &Ring) {
        self.health
            .log(route_id, ring, self.scheduled, &mut self.reported_glitches);
    }
}

impl Drop for ProgramRoute {
    fn drop(&mut self) {
        drop(self.output.routes.remove(self.slot));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use spaghetti_common::ipc_protocol::{FlowState, SrtMode, SrtSocket};

    use super::*;
    use crate::codec::tests::measure;
    use crate::router::Router;
    use crate::spec::{Destination, SourceSpec};

    const RATE: f64 = 48000.0;
    const BUFFER: usize = 480;

    fn program_source(router: &mut Router, input: &str) -> u32 {
        router
            .create_source(
                SourceSpec::Program {
                    input: input.into(),
                },
                None,
            )
            .unwrap()
    }

    fn to(output: &str, delay_ms: Option<u32>) -> Destination {
        Destination::Program {
            output: output.into(),
            delay_ms,
        }
    }

    fn program_route(router: &mut Router, source: u32, output: &str, delay_ms: Option<u32>) -> u32 {
        router
            .create_route(source, to(output, delay_ms), 1.0)
            .unwrap()
    }

    fn at(origin: HostTime, seconds: f64) -> HostTime {
        HostTime::from_ticks(origin.ticks() + (seconds * host_time::ticks_per_second()) as u64)
    }

    fn playback(router: &Router, route: u32) -> Option<Playback> {
        router
            .list_routes()
            .into_iter()
            .find(|listed| listed.route_id == route)
            .and_then(|listed| listed.playback)
    }

    /// Claude: pushes `samples(frame)` from `first` on, captured from `origin`
    /// on at `rate`, in callbacks of `BUFFER` frames.
    fn push_mono(
        input: &mut ProgramInput,
        origin: HostTime,
        rate: f64,
        frames: std::ops::Range<usize>,
        samples: impl Fn(usize) -> f32,
    ) {
        let all: Vec<f32> = frames.clone().map(&samples).collect();
        for (chunk, block) in all.chunks(BUFFER).enumerate() {
            let first = frames.start + chunk * BUFFER;
            input.push(
                Frames::mono(block).unwrap(),
                at(origin, first as f64 / rate),
            );
        }
    }

    /// Claude: renders `buffers` buffers back to back at `rate`, the first
    /// heard at `first_heard`, and returns both channels.
    fn render_at(
        output: &mut ProgramOutput,
        rate: f64,
        first_heard: HostTime,
        buffers: usize,
    ) -> (Vec<f32>, Vec<f32>) {
        let (mut played_left, mut played_right) = (Vec::new(), Vec::new());
        for buffer in 0..buffers {
            let (mut left, mut right) = ([0.0; BUFFER], [0.0; BUFFER]);
            let heard = at(first_heard, (buffer * BUFFER) as f64 / rate);
            output.render([&mut left, &mut right], heard).unwrap();
            played_left.extend_from_slice(&left);
            played_right.extend_from_slice(&right);
        }
        (played_left, played_right)
    }

    fn render(output: &mut ProgramOutput, first_heard: HostTime, buffers: usize) -> Vec<f32> {
        let (left, right) = render_at(output, RATE, first_heard, buffers);
        assert_eq!(left, right);
        left
    }

    fn loudest(samples: &[f32]) -> usize {
        (0..samples.len())
            .max_by(|&a, &b| samples[a].abs().total_cmp(&samples[b].abs()))
            .unwrap()
    }

    /// Claude: where in `played`, rendered from `first_heard` seconds on at
    /// `rate`, the click is heard, in seconds.
    fn heard_click(played: &[f32], rate: f64, first_heard: f64) -> f64 {
        first_heard + loudest(played) as f64 / rate
    }

    #[test]
    fn a_route_on_a_delay_plays_each_frame_its_delay_after_capture() {
        for latency in [Duration::ZERO, Duration::from_millis(50)] {
            let mut router = Router::new();
            let mut input = router.add_program_input("mic", RATE).unwrap();
            let mut output = router.add_program_output("speaker", RATE, latency).unwrap();
            let source = program_source(&mut router, "mic");
            program_route(&mut router, source, "speaker", Some(100));

            let origin = HostTime::now();
            let click = (0.2 * RATE) as usize;
            push_mono(&mut input, origin, RATE, 0..(0.5 * RATE) as usize, |f| {
                if f == click { 1.0 } else { 0.0 }
            });
            let played = render(&mut output, at(origin, 0.05), 50);
            let heard = heard_click(&played, RATE, 0.05);
            assert!(
                (heard - 0.3).abs() < 2.0 / RATE,
                "{latency:?}: the click was heard at {heard} s, not 0.3 s"
            );
        }
    }

    #[test]
    fn a_route_converts_the_input_rate_to_the_output_rate() {
        let input_rate = 44100.0;
        let mut router = Router::new();
        let mut input = router.add_program_input("mic", input_rate).unwrap();
        let mut output = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        let source = program_source(&mut router, "mic");
        program_route(&mut router, source, "speaker", Some(100));

        let origin = HostTime::now();
        let click = (0.6 * input_rate) as usize;
        push_mono(
            &mut input,
            origin,
            input_rate,
            0..input_rate as usize,
            |f| {
                if f == click {
                    1.0
                } else if f < (0.5 * input_rate) as usize {
                    0.5 * (2.0 * std::f64::consts::PI * 1000.0 * f as f64 / input_rate).sin() as f32
                } else {
                    0.0
                }
            },
        );
        let played = render(&mut output, at(origin, 0.1), 100);
        let tone: Vec<f32> = played[(0.1 * RATE) as usize..(0.4 * RATE) as usize]
            .iter()
            .flat_map(|&sample| [sample, sample])
            .collect();
        let (hz, rms) = measure(&tone, CHANNELS, RATE);
        assert!((hz - 1000.0).abs() < 2.0, "{hz} Hz");
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.01, "rms {rms}");
        let after_tone = (0.5 * RATE) as usize;
        let heard = heard_click(&played[after_tone..], RATE, 0.6);
        assert!(
            (heard - 0.7).abs() < 2.0 / RATE,
            "the click was heard at {heard} s, not 0.7 s"
        );
    }

    #[test]
    fn each_layout_plays_its_channels() {
        let (left, right) = ([0.25f32; BUFFER], [-0.5f32; BUFFER]);
        let interleaved: Vec<f32> = (0..BUFFER).flat_map(|_| [0.25, -0.5]).collect();
        let cases: [(Frames, (f32, f32)); 3] = [
            (Frames::mono(&left).unwrap(), (0.25, 0.25)),
            (Frames::interleaved(&interleaved).unwrap(), (0.25, -0.5)),
            (Frames::planar(&left, &right).unwrap(), (0.25, -0.5)),
        ];
        for (frames, expected) in cases {
            let mut router = Router::new();
            let mut input = router.add_program_input("mic", RATE).unwrap();
            let mut output = router
                .add_program_output("speaker", RATE, Duration::ZERO)
                .unwrap();
            let source = program_source(&mut router, "mic");
            program_route(&mut router, source, "speaker", Some(50));
            let origin = HostTime::now();
            for block in 0..40 {
                input.push(frames, at(origin, (block * BUFFER) as f64 / RATE));
            }
            let (played_left, played_right) = render_at(&mut output, RATE, at(origin, 0.05), 30);
            let steady = (0.15 * RATE) as usize..(0.25 * RATE) as usize;
            for (channel, played, level) in [
                ("left", &played_left, expected.0),
                ("right", &played_right, expected.1),
            ] {
                assert!(
                    played[steady.clone()]
                        .iter()
                        .all(|s| (s - level).abs() < 1e-3),
                    "{frames:?} {channel}: {:?}",
                    &played[steady.start..steady.start + 4]
                );
            }
        }
    }

    #[test]
    fn malformed_buffers_are_refused_whole() {
        assert_eq!(
            Frames::interleaved(&[0.0; 3]).unwrap_err(),
            BufferError::PartialFrame { samples: 3 }
        );
        assert_eq!(
            Frames::planar(&[0.0; 4], &[0.0; 3]).unwrap_err(),
            BufferError::UnequalPlanes { left: 4, right: 3 }
        );
        let long = vec![0.0; MAX_CALLBACK_FRAMES + 1];
        assert_eq!(
            Frames::mono(&long).unwrap_err(),
            BufferError::TooLong {
                frames: MAX_CALLBACK_FRAMES + 1
            }
        );
        assert_eq!(Frames::mono(&long[1..]).unwrap().len(), MAX_CALLBACK_FRAMES);

        let mut router = Router::new();
        let mut output = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        let (mut left, mut right) = (vec![1.0; 8], vec![1.0; 7]);
        assert_eq!(
            output.render([&mut left, &mut right], HostTime::now()),
            Err(BufferError::UnequalPlanes { left: 8, right: 7 })
        );
        assert!(left.iter().chain(&right).all(|s| *s == 0.0));
        let (mut left, mut right) = (long.clone(), long.clone());
        left.fill(1.0);
        assert_eq!(
            output.render([&mut left, &mut right], HostTime::now()),
            Err(BufferError::TooLong {
                frames: MAX_CALLBACK_FRAMES + 1
            })
        );
        assert!(left.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn routes_to_one_output_are_mixed() {
        let mut router = Router::new();
        let mut output = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        let origin = HostTime::now();
        for (name, level) in [("a", 0.25), ("b", 0.5)] {
            let mut input = router.add_program_input(name, RATE).unwrap();
            let source = program_source(&mut router, name);
            program_route(&mut router, source, "speaker", Some(50));
            push_mono(&mut input, origin, RATE, 0..(0.3 * RATE) as usize, |_| {
                level
            });
        }
        let played = render(&mut output, at(origin, 0.05), 25);
        let steady = &played[(0.15 * RATE) as usize..(0.25 * RATE) as usize];
        assert!(
            steady.iter().all(|s| (s - 0.75).abs() < 1e-3),
            "{:?}",
            &steady[..4]
        );
    }

    /// Claude: an audio device's I/O thread calling the program's callbacks in
    /// real time, a capture then a render every 10 ms, the way RemoteIO does.
    struct Device {
        stop: Arc<std::sync::atomic::AtomicBool>,
        played: Arc<Mutex<Vec<f32>>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Device {
        fn run(mut input: ProgramInput, mut output: ProgramOutput, level: f32) -> Self {
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let played = Arc::new(Mutex::new(Vec::new()));
            let thread = std::thread::spawn({
                let (stop, played) = (Arc::clone(&stop), Arc::clone(&played));
                move || {
                    let period = Duration::from_secs_f64(BUFFER as f64 / RATE);
                    let (start, origin) = (Instant::now(), HostTime::now());
                    let mut cycle = 0u32;
                    while !stop.load(Ordering::Relaxed) {
                        let captured = at(origin, (cycle as usize * BUFFER) as f64 / RATE);
                        input.push(Frames::mono(&[level; BUFFER]).unwrap(), captured);
                        let (mut left, mut right) = ([0.0; BUFFER], [0.0; BUFFER]);
                        output
                            .render([&mut left, &mut right], HostTime::now())
                            .unwrap();
                        played.lock().unwrap().extend_from_slice(&left);
                        cycle += 1;
                        if let Some(wait) =
                            (start + period * cycle).checked_duration_since(Instant::now())
                        {
                            std::thread::sleep(wait);
                        }
                    }
                }
            });
            Self {
                stop,
                played,
                thread: Some(thread),
            }
        }

        fn played(&self) -> Vec<f32> {
            self.played.lock().unwrap().clone()
        }
    }

    impl Drop for Device {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn wait_for(router: &Router, route: u32, wanted: Playback) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while playback(router, route) != Some(wanted) {
            assert!(Instant::now() < deadline, "{:?}", playback(router, route));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_route_removed_while_its_output_renders_fades_out_and_leaves_silence() {
        let mut router = Router::new();
        let input = router.add_program_input("mic", RATE).unwrap();
        let output = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        let source = program_source(&mut router, "mic");
        let route = program_route(&mut router, source, "speaker", None);
        let device = Device::run(input, output, 0.5);
        wait_for(&router, route, Playback::Playing);
        std::thread::sleep(Duration::from_millis(100));

        router.destroy_route(route).unwrap();
        let removed = device.played().len();
        std::thread::sleep(Duration::from_millis(100));
        let played = device.played();
        drop(device);

        let full = played.iter().rposition(|s| (s - 0.5).abs() < 1e-3).unwrap();
        let silent = full + played[full..].iter().position(|s| *s == 0.0).unwrap();
        assert!(
            silent - full >= (0.045 * RATE) as usize,
            "faded out over {} frames",
            silent - full
        );
        assert!(silent <= removed, "still played after the route was gone");
        assert!(played[silent..].iter().all(|s| *s == 0.0));
        assert!(
            played.len() - removed >= (0.08 * RATE) as usize,
            "the device stalled"
        );
    }

    #[test]
    fn playback_tells_playing_from_silent_muted_and_stopped() {
        let mut router = Router::new();
        let mut input = router.add_program_input("mic", RATE).unwrap();
        let mut output = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        let source = program_source(&mut router, "mic");
        let route = program_route(&mut router, source, "speaker", None);
        assert_eq!(playback(&router, route), Some(Playback::Stopped));

        let origin = HostTime::now();
        let mut pushed = 0;
        let mut cycle = |input: Option<&mut ProgramInput>, output: &mut ProgramOutput| {
            if let Some(input) = input {
                let captured = at(origin, pushed as f64 / RATE);
                input.push(Frames::mono(&[0.5; BUFFER]).unwrap(), captured);
                pushed += BUFFER;
            }
            let (mut left, mut right) = ([0.0; BUFFER], [0.0; BUFFER]);
            output
                .render([&mut left, &mut right], HostTime::now())
                .unwrap();
            left
        };
        cycle(None, &mut output);
        assert_eq!(playback(&router, route), Some(Playback::Silent));
        for _ in 0..10 {
            cycle(Some(&mut input), &mut output);
        }
        assert_eq!(playback(&router, route), Some(Playback::Playing));

        router.set_volume(route, 0.0).unwrap();
        for _ in 0..10 {
            cycle(Some(&mut input), &mut output);
        }
        assert!(
            cycle(Some(&mut input), &mut output)
                .iter()
                .all(|s| *s == 0.0)
        );
        assert_eq!(playback(&router, route), Some(Playback::Playing), "muted");

        let mut drained = [1.0; BUFFER];
        for _ in 0..10 {
            drained = cycle(None, &mut output);
        }
        assert!(drained.iter().all(|s| *s == 0.0));
        assert_eq!(playback(&router, route), Some(Playback::Silent), "drained");

        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(playback(&router, route), Some(Playback::Stopped));
    }

    #[test]
    fn a_refused_render_is_the_latest_render_and_plays_nothing() {
        let mut router = Router::new();
        let mut input = router.add_program_input("mic", RATE).unwrap();
        let mut output = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        let source = program_source(&mut router, "mic");
        let route = program_route(&mut router, source, "speaker", Some(50));
        let origin = HostTime::now();
        push_mono(&mut input, origin, RATE, 0..(0.3 * RATE) as usize, |_| 0.5);
        render(&mut output, at(origin, 0.05), 10);
        assert_eq!(playback(&router, route), Some(Playback::Playing));

        let (mut left, mut right) = ([1.0; BUFFER], [1.0; BUFFER - 1]);
        assert!(
            output
                .render([&mut left, &mut right], at(origin, 0.15))
                .is_err()
        );
        assert_ne!(playback(&router, route), Some(Playback::Playing));
        render(&mut output, at(origin, 0.16), 2);
        assert_eq!(playback(&router, route), Some(Playback::Playing));
    }

    /// Claude: audio dated half and a whole turn of a 32-bit tick count ago
    /// is long past its time on a 100 ms delay, and never plays.
    #[test]
    fn audio_dated_long_ago_never_plays_on_a_delay() {
        for ago in [1u64 << 31, 1 << 32] {
            let mut router = Router::new();
            let mut input = router.add_program_input("mic", RATE).unwrap();
            let mut output = router
                .add_program_output("speaker", RATE, Duration::ZERO)
                .unwrap();
            let source = program_source(&mut router, "mic");
            let route = program_route(&mut router, source, "speaker", Some(100));
            let now = HostTime::now();
            let origin = HostTime::from_ticks(now.ticks() - ago);
            push_mono(&mut input, origin, RATE, 0..30 * BUFFER, |_| 0.5);
            let played = render(&mut output, now, 20);
            let peak = played.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
            assert_eq!(peak, 0.0, "audio dated {ago} ticks ago played");
            assert_ne!(playback(&router, route), Some(Playback::Playing));
        }
    }

    #[test]
    fn a_resumed_output_plays_on_schedule_and_nothing_stale() {
        let mut router = Router::new();
        let mut input = router.add_program_input("mic", RATE).unwrap();
        let mut output = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        let source = program_source(&mut router, "mic");
        let route = program_route(&mut router, source, "speaker", Some(200));
        let origin = HostTime::now();
        let captured_at = |frame: usize| (frame as f64 / RATE) as f32;
        push_mono(
            &mut input,
            origin,
            RATE,
            0..(1.0 * RATE) as usize,
            captured_at,
        );
        render(&mut output, at(origin, 0.2), 20);
        assert_eq!(playback(&router, route), Some(Playback::Playing));

        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(playback(&router, route), Some(Playback::Stopped));
        let played = render(&mut output, at(origin, 0.9), 20);
        assert_eq!(playback(&router, route), Some(Playback::Playing));
        let heard_from = (0.1 * RATE) as usize;
        for (frame, sample) in played.iter().enumerate().skip(heard_from) {
            let captured = 0.9 + frame as f64 / RATE - 0.2;
            assert!(
                (f64::from(*sample) - captured).abs() < 1e-3,
                "heard at {} s: audio captured at {sample} s, not {captured} s",
                0.9 + frame as f64 / RATE
            );
        }
    }

    #[test]
    fn audio_from_before_an_overflow_never_plays_after_what_follows_it() {
        let mut router = Router::new();
        let mut input = router.add_program_input("mic", RATE).unwrap();
        let mut output = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        let source = program_source(&mut router, "mic");
        let route = program_route(&mut router, source, "speaker", None);
        let ring = router.route_ring(route);
        let origin = HostTime::now();
        let mut pushed = 0usize;
        let mut push = |input: &mut ProgramInput, level: f32| {
            input.push(
                Frames::mono(&[level; BUFFER]).unwrap(),
                at(origin, pushed as f64 / RATE),
            );
            pushed += BUFFER;
        };
        let render_now = |output: &mut ProgramOutput| {
            let (mut left, mut right) = ([0.0; BUFFER], [0.0; BUFFER]);
            output
                .render([&mut left, &mut right], HostTime::now())
                .unwrap();
            left
        };
        for _ in 0..20 {
            push(&mut input, 0.25);
            render_now(&mut output);
        }
        assert_eq!(playback(&router, route), Some(Playback::Playing));

        let mut stopped = 0;
        while ring.overflows() == 0 {
            push(&mut input, 0.25);
            stopped += 1;
            assert!(stopped < 1000);
        }
        let mut played = Vec::new();
        for _ in 0..50 {
            push(&mut input, -0.25);
            played.extend(render_now(&mut output));
        }
        let first_after = played
            .iter()
            .position(|s| *s < -0.1)
            .expect("what followed played");
        let stale = played[first_after..]
            .iter()
            .copied()
            .fold(f32::MIN, f32::max);
        assert!(
            stale < 0.01,
            "audio from before the overflow played after what followed it: {stale}"
        );
        assert!(
            first_after < (0.1 * RATE) as usize,
            "{first_after} frames of the backlog played"
        );
    }

    #[test]
    fn a_gap_in_capture_drops_what_was_queued_before_it_and_keeps_the_schedule() {
        let mut router = Router::new();
        let mut input = router.add_program_input("mic", RATE).unwrap();
        let mut output = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        let source = program_source(&mut router, "mic");
        program_route(&mut router, source, "speaker", Some(100));
        let origin = HostTime::now();
        let click = |at: f64, level: f32| {
            move |frame: usize| {
                if frame == (at * RATE) as usize {
                    1.0
                } else {
                    level
                }
            }
        };
        push_mono(
            &mut input,
            origin,
            RATE,
            0..(0.2 * RATE) as usize,
            click(0.1, 0.1),
        );
        let before = render(&mut output, at(origin, 0.05), 20);
        assert!((heard_click(&before, RATE, 0.05) - 0.2).abs() < 2.0 / RATE);

        push_mono(
            &mut input,
            origin,
            RATE,
            (0.25 * RATE) as usize..(0.6 * RATE) as usize,
            click(0.4, -0.1),
        );
        let after = render(&mut output, at(origin, 0.25), 40);
        let heard = heard_click(&after, RATE, 0.25);
        assert!(
            (heard - 0.5).abs() < 2.0 / RATE,
            "the click was heard at {heard} s"
        );
        let around_click =
            ((heard - 0.25 - 0.005) * RATE) as usize..((heard - 0.25 + 0.005) * RATE) as usize;
        let stale = after
            .iter()
            .enumerate()
            .filter(|(frame, _)| !around_click.contains(frame))
            .map(|(_, sample)| *sample)
            .fold(f32::MIN, f32::max);
        assert!(
            stale < 0.01,
            "what was captured before the gap played after it: {stale}"
        );
    }

    #[test]
    fn a_marked_discontinuity_drops_what_was_queued_before_it() {
        let mut router = Router::new();
        let mut input = router.add_program_input("mic", RATE).unwrap();
        let mut output = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        let source = program_source(&mut router, "mic");
        program_route(&mut router, source, "speaker", Some(100));
        let origin = HostTime::now();
        push_mono(&mut input, origin, RATE, 0..(0.2 * RATE) as usize, |_| 0.25);
        input.discontinuity();
        push_mono(
            &mut input,
            origin,
            RATE,
            (0.2 * RATE) as usize..(0.4 * RATE) as usize,
            |_| -0.25,
        );
        let played = render(&mut output, at(origin, 0.1), 40);
        let stale = played.iter().copied().fold(f32::MIN, f32::max);
        assert!(
            stale < 0.01,
            "audio from before the discontinuity played: {stale}"
        );
        assert!(played.iter().any(|s| *s < -0.2));
    }

    #[test]
    fn a_delay_is_refused_when_the_ring_cannot_hold_it() {
        let mut router = Router::new();
        let _input = router.add_program_input("mic", RATE).unwrap();
        let _fast = router.add_program_input("fast", 192_000.0).unwrap();
        let _speaker = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        let _late = router
            .add_program_output("late", RATE, Duration::from_secs(9))
            .unwrap();
        let mic = program_source(&mut router, "mic");
        let fast = program_source(&mut router, "fast");
        // Claude: 131072 frames hold 2.73 s at 48 kHz and 0.68 s at 192 kHz,
        // less what the resampler and the schedule keep in reserve.
        for (source, output, delay_ms, fits) in [
            (mic, "speaker", 2500, true),
            (mic, "speaker", 2800, false),
            (mic, "speaker", 10_000, false),
            (mic, "late", 10_000, true),
            (fast, "speaker", 500, true),
            (fast, "speaker", 700, false),
        ] {
            let created = router.create_route(source, to(output, Some(delay_ms)), 1.0);
            assert_eq!(
                created.is_ok(),
                fits,
                "{delay_ms} ms to {output}: {created:?}"
            );
        }
    }

    #[test]
    fn an_output_mixes_a_bounded_number_of_routes() {
        let mut router = Router::new();
        let _output = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        let mut inputs = Vec::new();
        for route in 0..=MAX_PROGRAM_ROUTES {
            let name = format!("mic {route}");
            inputs.push(router.add_program_input(&name, RATE).unwrap());
            let source = program_source(&mut router, &name);
            let created = router.create_route(source, to("speaker", None), 1.0);
            assert_eq!(created.is_ok(), route < MAX_PROGRAM_ROUTES, "route {route}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_srt_source_cannot_play_to_a_program_output_on_a_delay() {
        let mut router = Router::new();
        let _output = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        let source = router
            .create_source(
                SourceSpec::Srt(SrtSocket {
                    mode: SrtMode::Listen { port },
                    latency_ms: 120,
                    passphrase: None,
                    key_size: None,
                }),
                None,
            )
            .unwrap();
        let refused = router.create_route(source, to("speaker", Some(200)), 1.0);
        assert!(refused.unwrap_err().contains("does not date"));
        assert!(
            router
                .create_route(source, to("speaker", None), 1.0)
                .is_ok()
        );
    }

    #[test]
    fn names_that_are_not_registered_are_refused() {
        let mut router = Router::new();
        let refused = router.create_source(
            SourceSpec::Program {
                input: "mic".into(),
            },
            None,
        );
        assert!(refused.is_err());

        let _input = router.add_program_input("mic", RATE).unwrap();
        assert!(router.add_program_input("mic", RATE).is_err());
        assert!(
            router
                .add_program_output("speaker", f64::NAN, Duration::ZERO)
                .is_err()
        );
        let source = program_source(&mut router, "mic");
        assert!(
            router
                .create_route(source, to("speaker", None), 1.0)
                .is_err()
        );
    }

    #[test]
    fn removing_an_output_destroys_its_routes_and_leaves_its_render_silent() {
        let mut router = Router::new();
        let mut input = router.add_program_input("mic", RATE).unwrap();
        let mut output = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        let source = program_source(&mut router, "mic");
        program_route(&mut router, source, "speaker", Some(50));
        let origin = HostTime::now();
        push_mono(&mut input, origin, RATE, 0..(0.3 * RATE) as usize, |_| 0.5);
        assert!(
            render(&mut output, at(origin, 0.05), 10)
                .iter()
                .any(|s| *s != 0.0)
        );

        router.remove_program_output("speaker").unwrap();
        assert!(router.list_routes().is_empty());
        assert!(
            render(&mut output, at(origin, 0.15), 2)
                .iter()
                .all(|s| *s == 0.0)
        );
        assert!(
            router
                .create_route(source, to("speaker", None), 1.0)
                .is_err()
        );

        let mut replaced = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        program_route(&mut router, source, "speaker", Some(50));
        push_mono(
            &mut input,
            origin,
            RATE,
            (0.3 * RATE) as usize..(0.6 * RATE) as usize,
            |_| 0.5,
        );
        assert!(
            render(&mut output, at(origin, 0.4), 10)
                .iter()
                .all(|s| *s == 0.0)
        );
        assert!(
            render(&mut replaced, at(origin, 0.4), 10)
                .iter()
                .any(|s| *s != 0.0)
        );
    }

    #[test]
    fn removing_an_input_destroys_its_sources_and_its_pushes_feed_nothing() {
        let mut router = Router::new();
        let mut input = router.add_program_input("mic", RATE).unwrap();
        let _output = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        let source = program_source(&mut router, "mic");
        program_route(&mut router, source, "speaker", None);
        router.remove_program_input("mic").unwrap();
        assert!(router.list_sources().is_empty());
        assert!(router.list_routes().is_empty());
        input.push(Frames::mono(&[0.5; BUFFER]).unwrap(), HostTime::now());
        assert!(router.add_program_input("mic", RATE).is_ok());
    }

    #[test]
    fn a_source_idles_without_routes_and_flows_once_pushed_to_them() {
        let mut router = Router::new();
        let mut input = router.add_program_input("mic", RATE).unwrap();
        let _output = router
            .add_program_output("speaker", RATE, Duration::ZERO)
            .unwrap();
        let source = program_source(&mut router, "mic");
        let listed = |router: &Router| {
            router
                .list_sources()
                .into_iter()
                .find(|listed| listed.source_id == source)
                .unwrap()
        };
        assert_eq!(listed(&router).status.state, FlowState::Idle);
        assert_eq!(
            listed(&router).spec,
            SourceSpec::Program {
                input: "mic".into()
            }
        );
        assert_eq!(listed(&router).label, "mic");
        let route = program_route(&mut router, source, "speaker", Some(200));
        assert_eq!(listed(&router).status.state, FlowState::Connected);
        assert_eq!(
            router.list_routes()[0].destination,
            to("speaker", Some(200))
        );
        assert_eq!(playback(&router, route), Some(Playback::Stopped));
        input.push(
            Frames::mono(&[0.0; BUFFER]).unwrap(),
            HostTime::now() - Duration::from_millis(1),
        );
        assert_eq!(listed(&router).status.state, FlowState::Flowing);
    }
}
