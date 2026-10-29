use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use rubato::audioadapter_buffers::direct::{InterleavedSlice, SequentialSliceOfSlices};
use rubato::{
    Adjustable, Async, FixedAsync, Indexing, Resampler, Resizable, SincInterpolationParameters,
    WindowFunction,
};

use crate::delivery::DeliverySpan;
use crate::mixer::RAMP;
use crate::ring::{CHANNELS, LastPush, Ring};
use crate::sinks::SourceTiming;

pub const MAX_RENDER_FRAMES: usize = 4096;
const MAX_RATIO_RELATIVE: f64 = 1.01;
const SINC_LEN: usize = 128;
const MARGIN_SECONDS: f64 = 0.003;
const FILL_SMOOTHING_SECONDS: f64 = 1.0;
const RESYNC_RENDERS: usize = 4;
const MAX_CORRECTION: f64 = 0.005;
/// Claude: resampling this far off shifts pitch by 5 cents, about the smallest
/// change a listener hears.
const AUDIBLE_CORRECTION: f64 = 0.003;
const CONTROL_RADIANS_PER_SECOND: f64 = 0.1;
const CONTROL_DAMPING: f64 = 1.0;
/// Claude: a scheduled playout further off its schedule than this re-primes
/// rather than resample its way back.
const RESCHEDULE_SECONDS: f64 = 0.02;


/// Claude: a route that plays each frame `delay` seconds after the source
/// captured it, on a device that takes `latency` seconds from a render's
/// output time to sound. Routes with one delay play their source in step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Schedule {
    pub delay: f64,
    pub latency: f64,
}

/// Claude: a schedule in host ticks.
struct Scheduled {
    delay: f64,
    latency: f64,
    ticks_per_source_frame: f64,
    ticks_per_device_frame: f64,
    /// Claude: how many source frames behind its schedule the playout runs,
    /// smoothed over `FILL_SMOOTHING_SECONDS`.
    lateness_avg: f64,
}

/// Claude: the output side of a route. Resamples source-rate frames from the ring
/// to exactly the number of device-rate frames each render asks for, and keeps
/// the ring at a fill that survives the jitter between the two clock domains,
/// or, on a schedule, plays each frame when its capture time says. The
/// resampling ratio is trimmed by `FillController` so the source and device
/// clocks stay matched however far they drift apart.
pub struct Playout {
    resampler: Async<f32>,
    input: Vec<f32>,
    indexing: Indexing,
    device_rate: f64,
    source_frames_per_tick: f64,
    margin_frames: usize,
    delivery: Arc<DeliverySpan>,
    /// Claude: the span at the previous render.
    span: f64,
    primed: bool,
    fill_avg: f64,
    controller: FillController,
    schedule: Option<Scheduled>,
    fade_in_frames: usize,
    /// Claude: frames played since priming, up to `fade_in_frames`.
    faded: usize,
    health: Arc<PlayoutHealth>,
}

/// Claude: PI control of the ring fill. A positive correction makes the resampler
/// consume source frames faster. With fill dynamics `d(fill)/dt = rate * (drift -
/// correction)`, these gains give a critically damped loop at
/// `CONTROL_RADIANS_PER_SECOND`, slow enough to ignore callback jitter.
struct FillController {
    kp: f64,
    ki: f64,
    integral: f64,
    correction: f64,
}

impl FillController {
    fn new(source_rate: f64) -> Self {
        Self {
            kp: 2.0 * CONTROL_DAMPING * CONTROL_RADIANS_PER_SECOND / source_rate,
            ki: CONTROL_RADIANS_PER_SECOND.powi(2) / source_rate,
            integral: 0.0,
            correction: 0.0,
        }
    }

    fn update(&mut self, error_frames: f64, seconds: f64) {
        let integral_limit = MAX_CORRECTION / self.ki;
        self.integral =
            (self.integral + error_frames * seconds).clamp(-integral_limit, integral_limit);
        self.correction = (self.kp * error_frames + self.ki * self.integral)
            .clamp(-MAX_CORRECTION, MAX_CORRECTION);
    }

    /// Claude: the largest fill error the controller makes up without resampling
    /// further off than `AUDIBLE_CORRECTION`.
    fn inaudible_error(&self) -> f64 {
        AUDIBLE_CORRECTION / self.kp
    }
}

#[derive(Clone, Copy)]
enum Glitch {
    Underrun,
    Reprime,
    Resync,
}

/// Claude: written by the render callback, read by the health logger.
pub struct PlayoutHealth {
    pub source_rate: f64,
    delivery: Arc<DeliverySpan>,
    fill_avg: AtomicU32,
    target: AtomicU32,
    correction_ppm: AtomicU32,
    underruns: AtomicU32,
    reprimes: AtomicU32,
    resyncs: AtomicU32,
    glitch_span: AtomicU32,
    glitch_target: AtomicU32,
    lateness: AtomicU32,
}

pub struct HealthReport {
    pub fill_avg_frames: f32,
    pub target_frames: u32,
    pub delivery_frames: f64,
    pub correction_ppm: f32,
    pub underruns: u32,
    /// Claude: re-primes on purpose, after the span grew under a playing route.
    pub reprimes: u32,
    pub resyncs: u32,
    /// Claude: the span and the target at the latest underrun, re-prime or resync.
    pub glitch_span_frames: u32,
    pub glitch_target_frames: u32,
    /// Claude: how many source frames a scheduled playout runs behind its schedule.
    pub lateness_frames: f32,
}

impl PlayoutHealth {
    pub fn report(&self) -> HealthReport {
        HealthReport {
            fill_avg_frames: f32::from_bits(self.fill_avg.load(Ordering::Relaxed)),
            target_frames: self.target.load(Ordering::Relaxed),
            delivery_frames: self.delivery.frames(),
            correction_ppm: f32::from_bits(self.correction_ppm.load(Ordering::Relaxed)),
            underruns: self.underruns.load(Ordering::Relaxed),
            reprimes: self.reprimes.load(Ordering::Relaxed),
            resyncs: self.resyncs.load(Ordering::Relaxed),
            glitch_span_frames: self.glitch_span.load(Ordering::Relaxed),
            glitch_target_frames: self.glitch_target.load(Ordering::Relaxed),
            lateness_frames: f32::from_bits(self.lateness.load(Ordering::Relaxed)),
        }
    }

    fn glitched(&self, glitch: Glitch, span: f64, target: usize) {
        self.glitch_span
            .store(span.round() as u32, Ordering::Relaxed);
        self.glitch_target.store(target as u32, Ordering::Relaxed);
        let count = match glitch {
            Glitch::Underrun => &self.underruns,
            Glitch::Reprime => &self.reprimes,
            Glitch::Resync => &self.resyncs,
        };
        count.fetch_add(1, Ordering::Relaxed);
    }
}

impl Playout {
    /// Claude: the jitter buffer holds the source's delivery span on top of what
    /// the resampler needs, following the span as the source measures it. A
    /// scheduled playout ignores the span and plays by its schedule instead.
    pub fn new(
        source: &SourceTiming,
        device_rate: f64,
        ticks_per_second: f64,
        schedule: Option<Schedule>,
    ) -> Result<Self, String> {
        let source_rate = source.sample_rate;
        let params = SincInterpolationParameters::new(SINC_LEN, WindowFunction::BlackmanHarris2);
        let resampler = Async::<f32>::new_sinc(
            device_rate / source_rate,
            MAX_RATIO_RELATIVE,
            &params,
            MAX_RENDER_FRAMES,
            CHANNELS,
            FixedAsync::Output,
        )
        .map_err(|e| format!("create resampler: {e}"))?;
        let input = vec![0.0; resampler.input_frames_max() * CHANNELS];
        Ok(Self {
            resampler,
            input,
            indexing: Indexing::default(),
            device_rate,
            source_frames_per_tick: source_rate / ticks_per_second,
            margin_frames: (MARGIN_SECONDS * source_rate).ceil() as usize,
            delivery: Arc::clone(&source.delivery),
            span: source.delivery.frames(),
            primed: false,
            fill_avg: 0.0,
            controller: FillController::new(source_rate),
            fade_in_frames: (RAMP.as_secs_f64() * source_rate).ceil() as usize,
            faded: 0,
            schedule: schedule.map(|schedule| Scheduled {
                delay: schedule.delay * ticks_per_second,
                latency: schedule.latency * ticks_per_second,
                ticks_per_source_frame: ticks_per_second / source_rate,
                ticks_per_device_frame: ticks_per_second / device_rate,
                lateness_avg: 0.0,
            }),
            health: Arc::new(PlayoutHealth {
                source_rate,
                delivery: Arc::clone(&source.delivery),
                fill_avg: AtomicU32::new(0),
                target: AtomicU32::new(0),
                correction_ppm: AtomicU32::new(0),
                underruns: AtomicU32::new(0),
                reprimes: AtomicU32::new(0),
                resyncs: AtomicU32::new(0),
                glitch_span: AtomicU32::new(0),
                glitch_target: AtomicU32::new(0),
                lateness: AtomicU32::new(0),
            }),
        })
    }

    pub fn health(&self) -> Arc<PlayoutHealth> {
        Arc::clone(&self.health)
    }

    pub fn delay_frames(&self) -> usize {
        self.resampler.output_delay()
    }

    /// Claude: whether the latest render played the ring rather than priming silence.
    pub fn is_playing(&self) -> bool {
        self.primed
    }

    /// Claude: both channel slices must have the same length. `now` is the host
    /// time in the same ticks the producer stamps its pushes with, and `output`
    /// the host time at which the device outputs the render's first frame.
    pub fn render(&mut self, ring: &Ring, out: &mut [&mut [f32]; CHANNELS], now: u64, output: u64) {
        let frames = out[0].len();
        if frames == 0
            || frames > MAX_RENDER_FRAMES
            || (self.resampler.output_frames_next() != frames
                && self.resampler.set_chunk_size(frames).is_err())
        {
            silence(out);
            return;
        }
        if self.schedule.is_some() {
            self.render_scheduled(ring, out, now, output);
        } else {
            self.render_delivered(ring, out, now);
        }
    }

    fn render_delivered(&mut self, ring: &Ring, out: &mut [&mut [f32]; CHANNELS], now: u64) {
        let frames = out[0].len();
        let need = self.resampler.input_frames_next();
        let last = ring.last_push();
        // Claude: read after the ring, so the span a source publishes before a push
        // is seen with that push.
        let span = self.delivery.frames();
        let grown = span - self.span > self.controller.inaudible_error();
        self.span = span;
        let target = need + self.margin_frames + span.round() as usize;
        let fill = self.estimated_fill(&last, now, span);
        self.health.target.store(target as u32, Ordering::Relaxed);

        if !self.primed {
            if fill < target as f64 {
                silence(out);
                return;
            }
            ring.discard((fill - target as f64) as usize);
            self.primed = true;
            self.faded = 0;
            self.fill_avg = target as f64;
        } else if ring.available() < need {
            self.reprime(Glitch::Underrun, span, target, out);
            return;
        } else if grown {
            // Claude: the span grew under a playing route. Resampling up to the new
            // target would be audible, so the playout re-primes to it once instead.
            self.reprime(Glitch::Reprime, span, target, out);
            return;
        } else if fill > (target + RESYNC_RENDERS * need) as f64 + span {
            ring.discard((fill - target as f64) as usize);
            self.health.glitched(Glitch::Resync, span, target);
            self.fill_avg = target as f64;
        } else {
            let seconds = frames as f64 / self.device_rate;
            self.fill_avg += seconds / FILL_SMOOTHING_SECONDS * (fill - self.fill_avg);
            self.controller
                .update(self.fill_avg - target as f64, seconds);
        }
        self.health
            .fill_avg
            .store((self.fill_avg as f32).to_bits(), Ordering::Relaxed);
        self.health.correction_ppm.store(
            ((self.controller.correction * 1e6) as f32).to_bits(),
            Ordering::Relaxed,
        );

        let got = ring.pop_into(&mut self.input[..need * CHANNELS]);
        self.fade_in(0, got);
        self.indexing.partial_len = (got < need).then_some(got);
        if !self.resample(need, out) {
            silence(out);
        }
        // Claude: a new ratio changes how many input frames the resampler wants, so it
        // is applied after this render consumed exactly the `need` frames it asked for.
        let _ = self
            .resampler
            .set_resample_ratio_relative(1.0 / (1.0 + self.controller.correction), true);
    }

    /// Claude: plays the frame at the ring's tail when the device makes it heard
    /// `delay` after its capture. Unprimed, it drops the frames already too late
    /// and starts the earliest one mid-render behind silence, so it plays on
    /// schedule to the frame. Primed, the controller trims the ratio against
    /// the lateness the way it trims a fill error.
    fn render_scheduled(
        &mut self,
        ring: &Ring,
        out: &mut [&mut [f32]; CHANNELS],
        now: u64,
        output: u64,
    ) {
        let frames = out[0].len();
        let need = self.resampler.input_frames_next();
        let schedule = self.schedule.as_ref().expect("a scheduled playout");
        let heard = output as f64
            + schedule.latency
            + self.resampler.output_delay() as f64 * schedule.ticks_per_device_frame;
        let lateness = ring
            .tail_captured(schedule.ticks_per_source_frame, now)
            .map(|captured| {
                (heard - captured as f64 - schedule.delay) / schedule.ticks_per_source_frame
            });
        let Some(lateness) = lateness else {
            if self.primed {
                self.reprime(Glitch::Resync, 0.0, 0, out);
            } else {
                silence(out);
            }
            return;
        };
        let mut lead = 0;
        if !self.primed {
            if lateness > 0.0 {
                let late = lateness.round() as usize;
                if ring.discard(late) < late {
                    silence(out);
                    return;
                }
            } else {
                lead = (-lateness).round() as usize;
            }
            if lead >= need || ring.available() < need - lead {
                silence(out);
                return;
            }
            self.primed = true;
            self.faded = 0;
            self.schedule.as_mut().expect("scheduled").lateness_avg = 0.0;
        } else if ring.available() < need {
            self.reprime(Glitch::Underrun, 0.0, 0, out);
            return;
        } else if lateness.abs() > RESCHEDULE_SECONDS * self.health.source_rate {
            self.reprime(Glitch::Resync, 0.0, 0, out);
            return;
        } else {
            let seconds = frames as f64 / self.device_rate;
            let schedule = self.schedule.as_mut().expect("scheduled");
            schedule.lateness_avg +=
                seconds / FILL_SMOOTHING_SECONDS * (lateness - schedule.lateness_avg);
            self.controller.update(schedule.lateness_avg, seconds);
        }
        let lateness_avg = self.schedule.as_ref().expect("scheduled").lateness_avg;
        self.health
            .lateness
            .store((lateness_avg as f32).to_bits(), Ordering::Relaxed);
        self.health.correction_ppm.store(
            ((self.controller.correction * 1e6) as f32).to_bits(),
            Ordering::Relaxed,
        );

        self.input[..lead * CHANNELS].fill(0.0);
        let got = ring.pop_into(&mut self.input[lead * CHANNELS..need * CHANNELS]);
        self.fade_in(lead, got);
        self.indexing.partial_len = (lead + got < need).then_some(lead + got);
        if !self.resample(need, out) {
            silence(out);
        }
        let _ = self
            .resampler
            .set_resample_ratio_relative(1.0 / (1.0 + self.controller.correction), true);
    }

    /// Claude: scales `frames` input frames from `from` on along the fade-in.
    fn fade_in(&mut self, from: usize, frames: usize) {
        let fading = frames.min(self.fade_in_frames.saturating_sub(self.faded));
        for frame in from..from + fading {
            self.faded += 1;
            let gain = self.faded as f32 / self.fade_in_frames as f32;
            for sample in &mut self.input[frame * CHANNELS..][..CHANNELS] {
                *sample *= gain;
            }
        }
    }

    fn reprime(
        &mut self,
        glitch: Glitch,
        span: f64,
        target: usize,
        out: &mut [&mut [f32]; CHANNELS],
    ) {
        self.primed = false;
        self.resampler.reset();
        self.health.glitched(glitch, span, target);
        silence(out);
    }

    /// Claude: frames buffered as of the latest push, plus the frames the producer
    /// has captured since then but not delivered yet, up to its delivery span.
    /// Unlike the raw fill this holds steady through pushes that land late.
    fn estimated_fill(&self, last: &LastPush, now: u64, span: f64) -> f64 {
        // Claude: a push that lands after `now` was read is stamped later than it.
        let ticks = ((now as u32).wrapping_sub(last.at) as i32).max(0);
        let since_push = f64::from(ticks) * self.source_frames_per_tick;
        last.buffered as f64 + since_push.min(span)
    }

    fn resample(&mut self, need: usize, out: &mut [&mut [f32]; CHANNELS]) -> bool {
        let frames = out[0].len();
        let [left, right] = out;
        let mut planes = [&mut **left, &mut **right];
        let Ok(input) = InterleavedSlice::new(&self.input[..need * CHANNELS], CHANNELS, need)
        else {
            return false;
        };
        let Ok(mut output) = SequentialSliceOfSlices::new_mut(&mut planes, CHANNELS, frames) else {
            return false;
        };
        self.resampler
            .process_into_buffer(&input, &mut output, Some(&self.indexing))
            .is_ok()
    }
}

fn silence(out: &mut [&mut [f32]; CHANNELS]) {
    for plane in out.iter_mut() {
        plane.fill(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::DeliveryMeter;

    const TONE_HZ: f64 = 440.0;
    const TICKS_PER_SECOND: f64 = 24_000_000.0;
    const RENDER_FRAMES: usize = 512;
    /// Claude: resampling this much off shifts pitch by 5 cents, about the
    /// smallest change a listener hears.
    const INAUDIBLE_PPM: f32 = 3000.0;

    /// Claude: one push as a route's ring receives it: `frames` frames of the
    /// source's media from frame `media` on, landing `arrival` seconds in.
    #[derive(Clone, Copy)]
    struct Push {
        media: usize,
        frames: usize,
        arrival: f64,
    }

    /// Claude: pushes of `frames` frames, each landing as its last frame is captured.
    fn on_time(rate: f64, frames: usize, seconds: f64) -> Vec<Push> {
        (0..)
            .map(|i| Push {
                media: i * frames,
                frames,
                arrival: ((i + 1) * frames) as f64 / rate,
            })
            .take_while(|push| push.arrival < seconds)
            .collect()
    }

    /// Claude: how long Wi-Fi, str0m and the decoder take to release a held burst,
    /// so that render callbacks land in the middle of one.
    const BURST_SECONDS: f64 = 0.005;

    /// Claude: the network holds every push due in `[start, start + seconds)` and
    /// releases them at its end over `BURST_SECONDS`, as Wi-Fi does after it was
    /// off-channel.
    fn stall(pushes: &mut [Push], start: f64, seconds: f64) {
        stall_released_over(pushes, start, seconds, BURST_SECONDS);
    }

    /// Claude: a stall whose held pushes land over `release` after its end, as
    /// srt-tokio's release timer and SRT retransmissions spread them.
    fn stall_released_over(pushes: &mut [Push], start: f64, seconds: f64, release: f64) {
        let held: Vec<usize> = (0..pushes.len())
            .filter(|&i| (start..start + seconds).contains(&pushes[i].arrival))
            .collect();
        for (k, &i) in held.iter().enumerate() {
            pushes[i].arrival = start + seconds + release * k as f64 / held.len() as f64;
        }
    }

    /// Claude: the `lost` units due from `at` on never arrive, and the ones after
    /// them arrive on time, as SRT drops what a stall made too late to retransmit.
    fn drop_units(pushes: &mut Vec<Push>, at: f64, lost: usize) {
        let first = pushes.iter().position(|push| push.arrival >= at).unwrap();
        pushes.drain(first..first + lost);
    }

    /// Claude: a fixed sequence of uniform numbers, so a random-looking path
    /// replays the same way every run.
    struct Lcg(u64);

    impl Lcg {
        fn uniform(&mut self, low: f64, high: f64) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            low + (high - low) * (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// Claude: the field's hotspot path, a home Wi-Fi hop and a phone hotspot hop:
    /// each stalls for 50 to 300 ms every 0.3 to 4 s and releases what it held
    /// over up to 100 ms, and every packet lands up to 15 ms late on top.
    fn hotspot_path(pushes: &mut [Push], seconds: f64, seed: u64) {
        let mut random = Lcg(seed);
        for _hop in 0..2 {
            let mut start = random.uniform(1.0, 3.0);
            while start < seconds {
                let length = random.uniform(0.05, 0.3);
                let release = random.uniform(0.005, 0.1);
                stall_released_over(pushes, start, length, release);
                start += length + random.uniform(0.3, 4.0);
            }
        }
        for push in pushes.iter_mut() {
            push.arrival += random.uniform(0.0, 0.015);
        }
    }

    /// Claude: the `lost` packets due from `at` on never arrive and the next one
    /// lands `hold` late, as str0m holds it. The source conceals the gap when that
    /// one arrives.
    fn conceal(pushes: &mut Vec<Push>, at: f64, lost: usize, hold: f64) {
        let first = pushes.iter().position(|push| push.arrival >= at).unwrap();
        pushes[first + lost].arrival += hold;
        pushes.drain(first..first + lost);
    }

    /// Claude: what the simulated source tells its routes before each push: its
    /// buffer, as a tap does, or the span a network source measures.
    enum Source {
        Cadence(Arc<DeliverySpan>),
        Measured(DeliveryMeter),
    }

    impl Source {
        fn cadence() -> Self {
            Self::Cadence(Arc::new(DeliverySpan::new(0.0)))
        }

        /// Claude: a WebRTC source, whose floor is the jitter str0m's reorder hold allows.
        fn webrtc() -> Self {
            Self::Measured(DeliveryMeter::new(48000.0, 0.1))
        }

        /// Claude: an SRT source, whose floor is srt-tokio's coarse release timer.
        fn srt() -> Self {
            Self::Measured(DeliveryMeter::new(48000.0, 0.025))
        }

        fn span(&self) -> Arc<DeliverySpan> {
            match self {
                Self::Cadence(span) => Arc::clone(span),
                Self::Measured(meter) => meter.span(),
            }
        }

        /// Claude: returns the host time, in seconds, the ring push is stamped with.
        fn publish(&mut self, push: &Push, rate: f64) -> f64 {
            match self {
                Self::Cadence(span) => {
                    span.publish(push.frames as f64);
                    push.arrival
                }
                Self::Measured(meter) => meter.record(
                    push.media as f64 / rate..(push.media + push.frames) as f64 / rate,
                    push.arrival,
                ),
            }
        }

        /// Claude: the media a network source fills before `push`, in pushes of at
        /// most its length.
        fn gap_before(&self, push: &Push, rate: f64) -> Vec<Push> {
            let Self::Measured(meter) = self else {
                return Vec::new();
            };
            let Some(gap) = meter.gap_before(push.media as f64 / rate) else {
                return Vec::new();
            };
            let mut media = (gap.start * rate).round() as usize;
            let mut fill = Vec::new();
            while media < push.media {
                let frames = push.frames.min(push.media - media);
                fill.push(Push {
                    media,
                    frames,
                    arrival: push.arrival,
                });
                media += frames;
            }
            fill
        }
    }

    /// Claude: a producer and a device clocked independently, advanced in simulated
    /// host time. `device_ppm` makes the device run fast relative to the source.
    struct Sim {
        ring: Ring,
        source: Source,
        playout: Playout,
        source_rate: f64,
        pushes: Vec<Push>,
        next_push: usize,
        next_render: f64,
        rendered: Vec<f32>,
        /// Claude: when the device output each render.
        render_times: Vec<f64>,
        render_period: f64,
        worst_correction_ppm: f32,
        /// Claude: whether pushes carry their media time as their capture time.
        dated: bool,
    }

    impl Sim {
        fn new(source_rate: f64, device_rate: f64, push_frames: usize, device_ppm: f64) -> Self {
            let pushes = on_time(source_rate, push_frames, 100.0);
            Self::delivering(
                source_rate,
                device_rate,
                device_ppm,
                Source::cadence(),
                pushes,
            )
        }

        /// Claude: each push lands `lateness[i]` seconds after its regular time.
        fn late(
            source_rate: f64,
            device_rate: f64,
            push_frames: usize,
            device_ppm: f64,
            source: Source,
            lateness: &[f64],
        ) -> Self {
            let mut pushes = on_time(source_rate, push_frames, 100.0);
            for (push, late) in pushes.iter_mut().zip(lateness.iter().cycle()) {
                push.arrival += late;
            }
            Self::delivering(source_rate, device_rate, device_ppm, source, pushes)
        }

        fn delivering(
            source_rate: f64,
            device_rate: f64,
            device_ppm: f64,
            source: Source,
            pushes: Vec<Push>,
        ) -> Self {
            let timing = SourceTiming {
                sample_rate: source_rate,
                delivery: source.span(),
            };
            Self {
                ring: Ring::new(),
                playout: Playout::new(&timing, device_rate, TICKS_PER_SECOND, None).unwrap(),
                source,
                source_rate,
                pushes,
                next_push: 0,
                next_render: 0.0013,
                rendered: Vec::new(),
                render_times: Vec::new(),
                render_period: RENDER_FRAMES as f64 / (device_rate * (1.0 + device_ppm * 1e-6)),
                worst_correction_ppm: 0.0,
                dated: false,
            }
        }

        /// Claude: a source that dates each frame by its media time, played on
        /// `schedule`. The device's first render comes at `start`.
        fn scheduled(
            source_rate: f64,
            device_rate: f64,
            device_ppm: f64,
            schedule: Schedule,
            pushes: Vec<Push>,
            start: f64,
        ) -> Self {
            let mut sim = Self::delivering(
                source_rate,
                device_rate,
                device_ppm,
                Source::webrtc(),
                pushes,
            );
            let timing = SourceTiming {
                sample_rate: source_rate,
                delivery: sim.source.span(),
            };
            sim.playout =
                Playout::new(&timing, device_rate, TICKS_PER_SECOND, Some(schedule)).unwrap();
            sim.dated = true;
            sim.next_render = start;
            sim
        }

        /// Claude: how late, in seconds, the last `seconds` of output sounded
        /// against `schedule`, from the tone's phase: each sample should carry the
        /// tone as captured `schedule.delay` before it sounds.
        fn schedule_error(&self, schedule: Schedule, seconds: f64) -> f64 {
            let renders = (seconds / self.render_period) as usize;
            let first = self.render_times.len() - renders;
            let frame = self.render_period / RENDER_FRAMES as f64;
            let omega = 2.0 * std::f64::consts::PI * TONE_HZ;
            let (mut i, mut q) = (0.0, 0.0);
            for (render, &output) in self.render_times.iter().enumerate().skip(first) {
                let samples = &self.rendered[render * RENDER_FRAMES..][..RENDER_FRAMES];
                for (j, &sample) in samples.iter().enumerate() {
                    let phase =
                        omega * (output + j as f64 * frame + schedule.latency - schedule.delay);
                    i += f64::from(sample) * phase.sin();
                    q += f64::from(sample) * phase.cos();
                }
            }
            (-q).atan2(i) / omega
        }

        /// Claude: the producer stops for `seconds` and resumes where it left off.
        fn pause_producer(&mut self, seconds: f64) {
            for push in &mut self.pushes[self.next_push..] {
                push.arrival += seconds;
            }
        }

        /// Claude: gaps (underruns and re-primes to a grown target), resyncs and overflows.
        fn glitches(&self) -> [u32; 3] {
            let report = self.playout.health.report();
            [
                report.underruns + report.reprimes,
                report.resyncs,
                self.ring.overflows(),
            ]
        }

        /// Claude: asserts the playout holds what it held for a source of this static
        /// delivery jitter before sources measured their span: what it needs, the
        /// largest push, and the margin plus that jitter. What the resampler needs
        /// moves by a frame with the controller's ratio between renders.
        fn holds_the_static_target(&self, delivery_jitter: f64, push_frames: usize) {
            let report = self.playout.health.report();
            let expected = self.playout.resampler.input_frames_next()
                + push_frames
                + ((MARGIN_SECONDS + delivery_jitter) * self.source_rate).ceil() as usize;
            assert!(
                (report.target_frames as usize).abs_diff(expected) <= 1,
                "target {} for {expected}",
                report.target_frames
            );
            let span = push_frames as f64 + delivery_jitter * self.source_rate;
            assert!(
                (report.delivery_frames - span).abs() < 1e-6,
                "span {}",
                report.delivery_frames
            );
        }

        fn run(&mut self, seconds: f64) {
            let end = self.next_render + seconds;
            while self.next_render < end {
                let due = self.pushes.get(self.next_push).copied();
                if let Some(due) = due.filter(|due| due.arrival <= self.next_render) {
                    self.next_push += 1;
                    for fill in self.source.gap_before(&due, self.source_rate) {
                        self.push(fill);
                    }
                    self.push(due);
                } else {
                    let (mut left, mut right) =
                        (vec![0.0; RENDER_FRAMES], vec![0.0; RENDER_FRAMES]);
                    let now = (self.next_render * TICKS_PER_SECOND) as u64;
                    self.playout
                        .render(&self.ring, &mut [&mut left, &mut right], now, now);
                    assert_eq!(left, right);
                    self.rendered.extend_from_slice(&left);
                    self.render_times.push(self.next_render);
                    self.next_render += self.render_period;
                    let correction = self.playout.health.report().correction_ppm.abs();
                    self.worst_correction_ppm = self.worst_correction_ppm.max(correction);
                }
            }
        }

        /// Claude: the tone at the push's media time. A gap the source fills gets the
        /// tone too, so the tone shows whether playout ever skipped.
        fn push(&mut self, push: Push) {
            let rate = self.source_rate;
            let stamp = self.source.publish(&push, rate);
            let captured = self
                .dated
                .then(|| (push.media as f64 / rate * TICKS_PER_SECOND) as u64);
            self.ring.push_with(
                push.frames,
                (stamp * TICKS_PER_SECOND) as u64,
                captured,
                |f, _| {
                    (0.5 * (2.0 * std::f64::consts::PI * TONE_HZ * (push.media + f) as f64 / rate)
                        .sin()) as f32
                },
            );
        }

        fn first_sound(&self) -> usize {
            self.rendered.iter().position(|s| s.abs() > 1e-3).unwrap()
        }
    }

    fn worst_recurrence_error(samples: &[f32], rate: f64) -> f64 {
        let k = 2.0 * (2.0 * std::f64::consts::PI * TONE_HZ / rate).cos();
        samples
            .windows(3)
            .map(|w| (w[2] as f64 - (k * w[1] as f64 - w[0] as f64)).abs())
            .fold(0.0, f64::max)
    }

    #[test]
    fn plays_a_continuous_tone_once_primed() {
        for (source, device, push) in [
            (48000.0, 48000.0, 512),
            (48000.0, 44100.0, 512),
            (44100.0, 48000.0, 470),
        ] {
            let mut sim = Sim::new(source, device, push, 0.0);
            sim.run(5.0);
            let steady = &sim.rendered[sim.first_sound() + 4096..];
            let peak = steady.iter().fold(0.0f32, |p, s| p.max(s.abs()));
            assert!(
                (peak - 0.5).abs() < 0.01,
                "{source} -> {device}: peak {peak}"
            );
            let error = worst_recurrence_error(steady, device);
            assert!(
                error < 1e-3,
                "{source} -> {device}: recurrence error {error}"
            );
            assert_eq!(sim.glitches(), [0; 3], "{source} -> {device}");
        }
    }

    #[test]
    fn stays_silent_until_the_target_fill_is_reached() {
        let mut sim = Sim::new(48000.0, 44100.0, 470, 0.0);
        sim.run(1.0);
        let target = sim.playout.health.report().target_frames as usize;
        assert!(target > RENDER_FRAMES * 48 / 44 + 470, "target {target}");
        let first_output = sim.rendered.iter().position(|s| *s != 0.0).unwrap();
        assert!(
            first_output >= RENDER_FRAMES,
            "played after {first_output} frames"
        );
        assert_eq!(
            first_output % RENDER_FRAMES,
            0,
            "playback starts mid-render"
        );
    }

    #[test]
    fn controller_holds_the_target_fill_against_clock_drift() {
        for (source, device, push, device_ppm) in [
            (48000.0, 48000.0, 512, 300.0),
            (48000.0, 48000.0, 512, -300.0),
            (48000.0, 44100.0, 512, 120.0),
        ] {
            let mut sim = Sim::new(source, device, push, device_ppm);
            sim.run(60.0);
            let mut worst_error = 0.0f32;
            for _ in 0..20 {
                sim.run(1.0);
                let report = sim.playout.health.report();
                worst_error =
                    worst_error.max((report.fill_avg_frames - report.target_frames as f32).abs());
            }
            let report = sim.playout.health.report();
            let label = format!("{source} -> {device} at {device_ppm} ppm");
            assert_eq!(sim.glitches(), [0; 3], "{label}");
            assert!(
                worst_error < 16.0,
                "{label}: fill strays {worst_error} frames from target"
            );
            assert!(
                (report.correction_ppm as f64 + device_ppm).abs() < 0.05 * device_ppm.abs(),
                "{label}: correction {} ppm",
                report.correction_ppm
            );
            let steady = &sim.rendered[sim.rendered.len() - (20.0 * device) as usize..];
            assert!(worst_recurrence_error(steady, device) < 1e-3, "{label}");
            sim.holds_the_static_target(0.0, push);
        }
    }

    #[test]
    fn a_measured_span_absorbs_pushes_that_land_late() {
        let lateness = [0.0, 0.012, 0.004, 0.0, 0.011, 0.002, 0.0, 0.013];
        let mut cadence = Sim::late(48000.0, 48000.0, 1024, 0.0, Source::cadence(), &lateness);
        cadence.run(20.0);
        assert!(
            cadence.glitches()[0] > 1,
            "late pushes starve a buffer of one push"
        );

        let unfloored = Source::Measured(DeliveryMeter::new(48000.0, 0.0));
        let mut learning = Sim::late(48000.0, 48000.0, 1024, 0.0, unfloored, &lateness);
        learning.run(1.0);
        let adapted = learning.glitches();
        learning.run(19.0);
        assert!(adapted[0] <= 1, "{adapted:?}");
        assert_eq!(learning.glitches(), adapted);

        let floored = Source::Measured(DeliveryMeter::new(48000.0, 0.025));
        let mut padded = Sim::late(48000.0, 48000.0, 1024, 0.0, floored, &lateness);
        padded.run(20.0);
        assert_eq!(padded.glitches(), [0; 3]);
        let steady = &padded.rendered[padded.first_sound() + 4096..];
        assert!(worst_recurrence_error(steady, 48000.0) < 1e-3);
    }

    /// Claude: Wi-Fi with AirDrop on goes off-channel for about 0.4 s every 0.5 to
    /// 1.5 s, then delivers what it held in one burst.
    fn wifi_stalls(pushes: &mut [Push], from: f64, until: f64) {
        let mut start = from;
        let stalls = [
            (0.36, 1.0),
            (0.4, 0.5),
            (0.33, 1.5),
            (0.41, 0.8),
            (0.38, 1.2),
        ];
        for (seconds, period) in stalls.iter().cycle() {
            if start >= until {
                break;
            }
            stall(pushes, start, *seconds);
            start += period;
        }
    }

    #[test]
    fn rides_out_periodic_wifi_stalls_after_one_reprime() {
        let mut pushes = on_time(48000.0, 960, 70.0);
        wifi_stalls(&mut pushes, 2.0, 62.0);
        let mut sim = Sim::delivering(48000.0, 48000.0, 40.0, Source::webrtc(), pushes);
        sim.run(2.1);
        sim.worst_correction_ppm = 0.0;
        sim.run(0.8);
        let adapted = sim.glitches();
        let from = sim.rendered.len();
        sim.run(60.0);
        assert!(
            adapted[0] <= 1,
            "gaps, resyncs, overflows while adapting: {adapted:?}"
        );
        assert_eq!(sim.glitches(), adapted, "gaps, resyncs, overflows");
        assert!(worst_recurrence_error(&sim.rendered[from..], 48000.0) < 1e-3);
        assert!(
            sim.worst_correction_ppm < INAUDIBLE_PPM,
            "{} ppm",
            sim.worst_correction_ppm
        );
    }

    #[test]
    fn concealed_losses_raise_the_target_only_until_they_stop() {
        let mut pushes = on_time(48000.0, 960, 210.0);
        let mut at = 2.0;
        while at < 30.0 {
            conceal(&mut pushes, at, 5, 0.04);
            at += 2.0;
        }
        let mut sim = Sim::delivering(48000.0, 48000.0, 40.0, Source::webrtc(), pushes);
        sim.run(2.9);
        let adapted = sim.glitches();
        let from = sim.rendered.len();
        sim.worst_correction_ppm = 0.0;
        sim.run(200.0);
        assert!(
            adapted[0] <= 1,
            "gaps, resyncs, overflows while adapting: {adapted:?}"
        );
        assert_eq!(sim.glitches(), adapted, "gaps, resyncs, overflows");
        assert!(worst_recurrence_error(&sim.rendered[from..], 48000.0) < 1e-3);
        assert!(
            sim.worst_correction_ppm < INAUDIBLE_PPM,
            "shedding the span resampled {} ppm fast",
            sim.worst_correction_ppm
        );
        sim.holds_the_static_target(0.1, 960);
    }

    #[test]
    fn a_quiet_network_source_keeps_the_static_target() {
        let mut pushes = on_time(48000.0, 960, 30.0);
        let lateness = [0.0, 0.003, 0.001, 0.004, 0.0, 0.002];
        for (push, late) in pushes.iter_mut().zip(lateness.iter().cycle()) {
            push.arrival += late;
        }
        let mut sim = Sim::delivering(48000.0, 48000.0, 40.0, Source::webrtc(), pushes);
        sim.run(25.0);
        assert_eq!(sim.glitches(), [0; 3]);
        sim.holds_the_static_target(0.1, 960);
    }

    #[test]
    fn the_ring_holds_the_burst_after_a_stall_at_the_largest_span() {
        let mut pushes = on_time(48000.0, 960, 50.0);
        let mut start = 2.0;
        while start < 44.0 {
            stall(&mut pushes, start, 0.75);
            start += 2.0;
        }
        let mut sim = Sim::delivering(48000.0, 48000.0, 40.0, Source::webrtc(), pushes);
        sim.run(3.9);
        let adapted = sim.glitches();
        sim.run(42.0);
        assert!(
            adapted[0] <= 1,
            "gaps, resyncs, overflows while adapting: {adapted:?}"
        );
        assert_eq!(sim.glitches(), adapted, "gaps, resyncs, overflows");
    }

    /// Claude: 0.4 s stalls `every` seconds for five minutes, as background Wi-Fi
    /// scans or a roam make them, cost the first stall's re-prime and no more.
    fn rides_out_rare_stalls(every: f64) {
        let mut pushes = on_time(48000.0, 960, 310.0);
        let mut start = 2.0;
        let mut stalls = 0;
        while start < 300.0 {
            stall(&mut pushes, start, 0.4);
            start += every;
            stalls += 1;
        }
        let mut sim = Sim::delivering(48000.0, 48000.0, 40.0, Source::webrtc(), pushes);
        sim.run(2.1);
        sim.worst_correction_ppm = 0.0;
        sim.run(0.8);
        let adapted = sim.glitches();
        sim.run(300.0);
        let label = format!("{stalls} stalls {every} s apart");
        assert!(adapted[0] <= 1, "{label}, while adapting: {adapted:?}");
        assert_eq!(sim.glitches(), adapted, "{label}: gaps, resyncs, overflows");
        assert!(
            sim.worst_correction_ppm < INAUDIBLE_PPM,
            "{label}: {} ppm",
            sim.worst_correction_ppm
        );
    }

    #[test]
    fn rides_out_stalls_15_s_apart() {
        rides_out_rare_stalls(15.0);
    }

    #[test]
    fn rides_out_stalls_30_s_apart() {
        rides_out_rare_stalls(30.0);
    }

    #[test]
    fn rides_out_stalls_60_s_apart() {
        rides_out_rare_stalls(60.0);
    }

    /// Claude: the relay pin's Wi-Fi: stalls of about 0.1 s every 0.45 s, with one
    /// of about 0.4 s a minute.
    #[test]
    fn rides_out_large_stalls_a_minute_apart_among_small_ones() {
        let mut pushes = on_time(48000.0, 960, 310.0);
        let mut small = 2.0;
        while small < 300.0 {
            stall(&mut pushes, small, 0.1);
            small += 0.45;
        }
        let mut large = 5.0;
        while large < 300.0 {
            stall(&mut pushes, large, 0.38);
            large += 60.0;
        }
        let mut sim = Sim::delivering(48000.0, 48000.0, 40.0, Source::webrtc(), pushes);
        sim.run(5.1);
        sim.worst_correction_ppm = 0.0;
        sim.run(1.0);
        let adapted = sim.glitches();
        sim.run(298.0);
        assert!(
            adapted[0] <= 2,
            "gaps, resyncs, overflows while adapting: {adapted:?}"
        );
        assert_eq!(sim.glitches(), adapted, "gaps, resyncs, overflows");
        assert!(
            sim.worst_correction_ppm < INAUDIBLE_PPM,
            "{} ppm",
            sim.worst_correction_ppm
        );
    }

    /// Claude: after stalls the span covers, one that outlasts it by `excess` costs
    /// one re-prime, and the controller never has to make up a grown target.
    #[test]
    fn a_stall_just_past_the_span_reprimes_once() {
        for excess in [0.002, 0.005, 0.012] {
            let mut pushes = on_time(48000.0, 960, 60.0);
            let mut start = 2.0;
            while start < 30.0 {
                stall(&mut pushes, start, 0.36);
                start += 1.0;
            }
            let mut sim = Sim::delivering(48000.0, 48000.0, 40.0, Source::webrtc(), pushes);
            sim.run(30.9);
            let covered = sim.glitches();
            let span = sim.playout.health.report().delivery_frames / 48000.0;
            let next = sim.next_push;
            // Claude: the first packet held from 31 s on waits the stall plus its own 20 ms.
            stall(&mut sim.pushes[next..], 31.0, span + excess - 0.02);
            sim.worst_correction_ppm = 0.0;
            sim.run(25.0);
            let [gaps, resyncs, overflows] = sim.glitches();
            let label = format!("{excess} s past a span of {span} s");
            assert_eq!(covered, [1, 0, 0], "{label}: before the stall");
            assert_eq!(
                [gaps - covered[0], resyncs, overflows],
                [1, 0, 0],
                "{label}: gaps, resyncs, overflows"
            );
            assert!(
                sim.worst_correction_ppm < INAUDIBLE_PPM,
                "{label}: {} ppm",
                sim.worst_correction_ppm
            );
        }
    }

    #[test]
    fn single_losses_at_the_floor_leave_the_target_alone() {
        let mut pushes = on_time(48000.0, 960, 70.0);
        let mut at = 2.0;
        while at < 60.0 {
            conceal(&mut pushes, at, 1, 0.04);
            at += 3.0;
        }
        let mut sim = Sim::delivering(48000.0, 48000.0, 40.0, Source::webrtc(), pushes);
        sim.run(1.0);
        sim.worst_correction_ppm = 0.0;
        let floor = sim.playout.health.report().target_frames;
        let mut highest = floor;
        for _ in 0..120 {
            sim.run(0.5);
            highest = highest.max(sim.playout.health.report().target_frames);
        }
        assert_eq!(sim.glitches(), [0; 3]);
        assert!(
            highest <= floor + 1,
            "target rose from {floor} to {highest}"
        );
        assert!(
            sim.worst_correction_ppm < INAUDIBLE_PPM,
            "{} ppm",
            sim.worst_correction_ppm
        );
    }

    /// Claude: srt-tokio and SRT retransmission release a held burst over tens to
    /// hundreds of milliseconds. Covered stalls released that slowly cost nothing,
    /// and one that outlasts the span costs one re-prime.
    #[test]
    fn slowly_released_bursts_reprime_only_when_the_span_grows() {
        for release in [0.05, 0.1, 0.2, 0.3] {
            let mut pushes = on_time(48000.0, 960, 50.0);
            let mut start = 2.0;
            while start < 40.0 {
                if start != 21.0 {
                    stall_released_over(&mut pushes, start, 0.36, release);
                }
                start += 1.0;
            }
            let mut sim = Sim::delivering(48000.0, 48000.0, 40.0, Source::webrtc(), pushes);
            sim.run(2.95);
            let adapted = sim.glitches();
            sim.worst_correction_ppm = 0.0;
            sim.run(17.9);
            let covered = sim.glitches();
            let span = sim.playout.health.report().delivery_frames / 48000.0;
            let next = sim.next_push;
            // Claude: the first packet held from 21 s on waits the stall plus its own 20 ms.
            stall_released_over(&mut sim.pushes[next..], 21.0, span + 0.005 - 0.02, release);
            sim.run(25.0);
            let label = format!("released over {release} s");
            assert_eq!(adapted, [1, 0, 0], "{label}: the first stall");
            assert_eq!(covered, adapted, "{label}: covered stalls");
            assert_eq!(
                sim.glitches(),
                [2, 0, 0],
                "{label}: a stall that outlasts the span"
            );
            assert!(
                sim.worst_correction_ppm < INAUDIBLE_PPM,
                "{label}: {} ppm",
                sim.worst_correction_ppm
            );
        }
    }

    /// Claude: holes SRT leaves in media time cost the silence of their own
    /// length once the span covers them. At the floor a unit's hole is covered; a
    /// two-unit hole starves the ring for three units, so the first one costs a
    /// gap and grows the span, and later ones cost nothing.
    #[test]
    fn srt_holes_cost_their_own_length_once_the_span_covers_them() {
        let mut pushes = on_time(48000.0, 1024, 70.0);
        let mut at = 5.0;
        for lost in [1, 2, 1, 2, 1, 2] {
            drop_units(&mut pushes, at, lost);
            at += 10.0;
        }
        let mut sim = Sim::delivering(48000.0, 48000.0, 40.0, Source::srt(), pushes);
        sim.run(1.0);
        let floor = sim.playout.health.report().delivery_frames;
        sim.worst_correction_ppm = 0.0;
        sim.run(13.9);
        assert_eq!(sim.glitches(), [0, 0, 0], "a hole of one unit at the floor");
        sim.run(2.0);
        assert_eq!(sim.glitches(), [1, 0, 0], "the first hole of two units");
        let from = sim.rendered.len();
        sim.run(52.0);
        assert_eq!(
            sim.glitches(),
            [1, 0, 0],
            "holes after the span covers them"
        );
        assert!(sim.playout.health.report().delivery_frames > floor);
        assert!(worst_recurrence_error(&sim.rendered[from..], 48000.0) < 1e-3);
        assert!(
            sim.worst_correction_ppm < INAUDIBLE_PPM,
            "{} ppm",
            sim.worst_correction_ppm
        );
    }

    /// Claude: three minutes over the hotspot path. Every re-prime comes with a
    /// grown span, and a few seconds after any gap the controller is back inside
    /// what a listener cannot hear.
    #[test]
    fn a_hotspot_path_reprimes_only_on_growth_and_settles_after_each_gap() {
        for seed in [1, 2, 3] {
            let mut pushes = on_time(48000.0, 960, 190.0);
            hotspot_path(&mut pushes, 185.0, seed);
            let mut sim = Sim::delivering(48000.0, 48000.0, 40.0, Source::webrtc(), pushes);
            let label = format!("seed {seed}");
            let (mut report, mut last_gap) = (sim.playout.health.report(), 0.0);
            while sim.next_render < 180.0 {
                sim.run(0.01);
                let now = sim.playout.health.report();
                if now.reprimes > report.reprimes {
                    assert!(
                        now.delivery_frames > report.delivery_frames,
                        "{label}: re-primed at {:.2} s at an unchanged span of {} frames",
                        sim.next_render,
                        report.delivery_frames
                    );
                }
                if now.underruns + now.reprimes > report.underruns + report.reprimes {
                    last_gap = sim.next_render;
                } else if sim.next_render > last_gap + 3.0 {
                    assert!(
                        now.correction_ppm.abs() < INAUDIBLE_PPM,
                        "{label}: {} ppm at {:.2} s, {:.2} s after a gap",
                        now.correction_ppm,
                        sim.next_render,
                        sim.next_render - last_gap
                    );
                }
                report = now;
            }
            assert_eq!(sim.glitches()[1..], [0, 0], "{label}: resyncs, overflows");
        }
    }

    /// Claude: a push can land between the render reading the host time and
    /// reading the ring, stamped just after that time. It is owed nothing yet,
    /// so it must not count as a whole span of captured media.
    #[test]
    fn a_push_stamped_after_the_render_is_owed_nothing() {
        let timing = SourceTiming {
            sample_rate: 48000.0,
            delivery: Arc::new(DeliverySpan::new(4800.0)),
        };
        let mut playout = Playout::new(&timing, 48000.0, TICKS_PER_SECOND, None).unwrap();
        let ring = Ring::new();
        let now = (10.0 * TICKS_PER_SECOND) as u64;
        ring.push_with(960, now + 1, None, |_, _| 0.25);
        let (mut left, mut right) = (vec![0.0; RENDER_FRAMES], vec![0.0; RENDER_FRAMES]);
        playout.render(&ring, &mut [&mut left, &mut right], now, now);
        assert!(
            !playout.is_playing(),
            "primed on 960 frames against a span of 4800"
        );
        assert_eq!(ring.available(), 960);
    }

    #[test]
    fn a_fast_source_at_the_largest_span_resyncs_before_the_ring_overflows() {
        let mut pushes = on_time(48000.0, 960, 100.0);
        let mut start = 2.0;
        while start < 98.0 {
            stall(&mut pushes, start, 0.75);
            start += 2.0;
        }
        let mut sim = Sim::delivering(48000.0, 48000.0, -30_000.0, Source::webrtc(), pushes);
        sim.run(95.0);
        let [_, resyncs, overflows] = sim.glitches();
        assert!(resyncs > 0, "the fill never ran past the span");
        assert_eq!(overflows, 0);
    }

    #[test]
    fn reprimes_after_the_producer_stalls() {
        let mut sim = Sim::new(48000.0, 48000.0, 512, 0.0);
        sim.run(1.0);
        sim.pause_producer(0.1);
        sim.run(1.0);
        let report = sim.playout.health.report();
        assert_eq!(report.underruns, 1);
        let tail = &sim.rendered[sim.rendered.len() - 8192..];
        assert!(worst_recurrence_error(tail, 48000.0) < 1e-3);
    }
    const SCHEDULE: Schedule = Schedule {
        delay: 0.25,
        latency: 0.012,
    };

    #[test]
    fn plays_each_frame_its_delay_after_capture() {
        for (source, device, push) in [
            (48000.0, 48000.0, 960),
            (48000.0, 44100.0, 960),
            (44100.0, 48000.0, 470),
        ] {
            let mut pushes = on_time(source, push, 100.0);
            let mut random = Lcg(7);
            let mut latest = 0.0f64;
            for push in &mut pushes {
                push.arrival = latest.max(push.arrival + random.uniform(0.0, 0.15));
                latest = push.arrival;
            }
            for start in [0.0013, 0.0071] {
                let mut sim = Sim::scheduled(source, device, 0.0, SCHEDULE, pushes.clone(), start);
                sim.run(10.0);
                let error = sim.schedule_error(SCHEDULE, 2.0);
                let label = format!("{source} -> {device}, first render at {start}");
                assert!(error.abs() < 50e-6, "{label}: {} µs off", error * 1e6);
                assert_eq!(sim.glitches(), [0; 3], "{label}");
            }
        }
    }

    #[test]
    fn holds_its_schedule_against_clock_drift() {
        for device_ppm in [300.0, -300.0] {
            let pushes = on_time(48000.0, 960, 100.0);
            let mut sim = Sim::scheduled(48000.0, 48000.0, device_ppm, SCHEDULE, pushes, 0.0013);
            sim.run(60.0);
            let mut worst = 0.0f64;
            for _ in 0..20 {
                sim.run(1.0);
                worst = worst.max(sim.schedule_error(SCHEDULE, 1.0).abs());
            }
            assert!(worst < 300e-6, "{device_ppm} ppm: {} µs off", worst * 1e6);
            assert!(sim.worst_correction_ppm < INAUDIBLE_PPM, "{device_ppm} ppm");
            assert_eq!(sim.glitches(), [0; 3], "{device_ppm} ppm");
        }
    }

    #[test]
    fn drops_what_lands_past_its_time_and_plays_on_schedule_after() {
        let mut pushes = on_time(48000.0, 960, 100.0);
        stall(&mut pushes, 3.0, 0.5);
        let mut sim = Sim::scheduled(48000.0, 48000.0, 0.0, SCHEDULE, pushes, 0.0013);
        sim.run(8.0);
        assert_eq!(sim.playout.health.report().underruns, 1);
        let error = sim.schedule_error(SCHEDULE, 2.0);
        assert!(error.abs() < 20e-6, "{} µs off", error * 1e6);
    }
    /// Claude: the output's peak over `frames` frames from `at`.
    fn peak(samples: &[f32], at: usize, frames: usize) -> f32 {
        samples[at..at + frames]
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()))
    }

    #[test]
    fn fades_in_when_it_starts_playing() {
        let delivered = Sim::new(48000.0, 48000.0, 512, 0.0);
        let scheduled = Sim::scheduled(
            48000.0,
            48000.0,
            0.0,
            SCHEDULE,
            on_time(48000.0, 960, 100.0),
            0.0071,
        );
        for (label, mut sim) in [("delivered", delivered), ("scheduled", scheduled)] {
            sim.run(2.0);
            let start = sim.rendered.iter().position(|s| *s != 0.0).unwrap();
            let opening = peak(&sim.rendered, start, 24);
            let halfway = peak(&sim.rendered, start + 1140, 120);
            let steady = peak(&sim.rendered, start + 4800, 960);
            assert!(
                opening < 0.1 * steady,
                "{label}: opens at {opening} of {steady}"
            );
            assert!(
                (0.4 * steady..0.6 * steady).contains(&halfway),
                "{label}: halfway at {halfway} of {steady}"
            );
            assert!((steady - 0.5).abs() < 0.01, "{label}: steady at {steady}");
        }
    }
}
