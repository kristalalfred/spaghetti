use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use rubato::audioadapter_buffers::direct::{InterleavedSlice, SequentialSliceOfSlices};
use rubato::{
    Async, FixedAsync, Indexing, Resampler, Resizable, SincInterpolationParameters, WindowFunction,
};

use crate::ring::{CHANNELS, Ring};

pub const MAX_RENDER_FRAMES: usize = 4096;
const MAX_RATIO_RELATIVE: f64 = 1.01;
const SINC_LEN: usize = 128;
const MARGIN_SECONDS: f64 = 0.003;
const FILL_SMOOTHING_SECONDS: f64 = 1.0;
const RESYNC_CHUNKS: usize = 4;

/// Claude: the output side of a route. Resamples source-rate frames from the ring
/// to exactly the number of device-rate frames each render asks for, and keeps
/// the ring at a fill that survives the jitter between the two clock domains.
pub struct Playout {
    resampler: Async<f32>,
    input: Vec<f32>,
    indexing: Indexing,
    device_rate: f64,
    source_frames_per_tick: f64,
    margin_frames: usize,
    primed: bool,
    fill_avg: f64,
    health: Arc<PlayoutHealth>,
}

/// Claude: written by the render callback, read by the health logger.
pub struct PlayoutHealth {
    pub source_rate: f64,
    fill_avg: AtomicU32,
    target: AtomicU32,
    underruns: AtomicU32,
    resyncs: AtomicU32,
}

pub struct HealthReport {
    pub fill_avg_frames: f32,
    pub target_frames: u32,
    pub underruns: u32,
    pub resyncs: u32,
}

impl PlayoutHealth {
    pub fn report(&self) -> HealthReport {
        HealthReport {
            fill_avg_frames: f32::from_bits(self.fill_avg.load(Ordering::Relaxed)),
            target_frames: self.target.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            resyncs: self.resyncs.load(Ordering::Relaxed),
        }
    }
}

impl Playout {
    pub fn new(source_rate: f64, device_rate: f64, ticks_per_second: f64) -> Result<Self, String> {
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
            primed: false,
            fill_avg: 0.0,
            health: Arc::new(PlayoutHealth {
                source_rate,
                fill_avg: AtomicU32::new(0),
                target: AtomicU32::new(0),
                underruns: AtomicU32::new(0),
                resyncs: AtomicU32::new(0),
            }),
        })
    }

    pub fn health(&self) -> Arc<PlayoutHealth> {
        Arc::clone(&self.health)
    }

    pub fn delay_frames(&self) -> usize {
        self.resampler.output_delay()
    }

    /// Claude: both channel slices must have the same length. `now` is the host
    /// time in the same ticks the producer stamps its pushes with.
    pub fn render(&mut self, ring: &Ring, out: &mut [&mut [f32]; CHANNELS], now: u64) {
        let frames = out[0].len();
        if frames == 0
            || frames > MAX_RENDER_FRAMES
            || (self.resampler.output_frames_next() != frames
                && self.resampler.set_chunk_size(frames).is_err())
        {
            silence(out);
            return;
        }

        let need = self.resampler.input_frames_next();
        let chunk = ring.largest_push();
        let target = need + chunk + self.margin_frames;
        let fill = self.estimated_fill(ring, now, chunk);
        self.health.target.store(target as u32, Ordering::Relaxed);

        if !self.primed {
            if fill < target as f64 {
                silence(out);
                return;
            }
            ring.discard((fill - target as f64) as usize);
            self.primed = true;
            self.fill_avg = target as f64;
        } else if ring.available() < need {
            self.primed = false;
            self.resampler.reset();
            self.health.underruns.fetch_add(1, Ordering::Relaxed);
            silence(out);
            return;
        } else if fill > (target + RESYNC_CHUNKS * chunk.max(need)) as f64 {
            ring.discard((fill - target as f64) as usize);
            self.health.resyncs.fetch_add(1, Ordering::Relaxed);
            self.fill_avg = target as f64;
        } else {
            let alpha = frames as f64 / (self.device_rate * FILL_SMOOTHING_SECONDS);
            self.fill_avg += alpha * (fill - self.fill_avg);
        }
        self.health
            .fill_avg
            .store((self.fill_avg as f32).to_bits(), Ordering::Relaxed);

        let got = ring.pop_into(&mut self.input[..need * CHANNELS]);
        self.indexing.partial_len = (got < need).then_some(got);
        if !self.resample(need, out) {
            silence(out);
        }
    }

    /// Claude: frames buffered as of the latest push, plus the frames the producer
    /// has captured since then but not delivered yet. Unlike the raw fill this
    /// does not jump by a whole producer chunk at each push.
    fn estimated_fill(&self, ring: &Ring, now: u64, chunk: usize) -> f64 {
        let last = ring.last_push();
        let since_push = (now as u32).wrapping_sub(last.at) as f64 * self.source_frames_per_tick;
        last.buffered as f64 + since_push.min(chunk as f64)
    }

    fn resample(&mut self, need: usize, out: &mut [&mut [f32]; CHANNELS]) -> bool {
        let frames = out[0].len();
        let [left, right] = out;
        let mut planes = [&mut **left, &mut **right];
        let Ok(input) = InterleavedSlice::new(&self.input[..need * CHANNELS], CHANNELS, need) else {
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

    const TONE_HZ: f64 = 440.0;
    const TICKS_PER_SECOND: f64 = 24_000_000.0;
    const RENDER_FRAMES: usize = 512;

    /// Claude: a producer and a device clocked independently, advanced in simulated
    /// host time. `device_ppm` makes the device run fast relative to the source.
    struct Sim {
        ring: Ring,
        playout: Playout,
        source_rate: f64,
        push_frames: usize,
        next_push: f64,
        next_render: f64,
        produced: usize,
        rendered: Vec<f32>,
        render_period: f64,
    }

    impl Sim {
        fn new(source_rate: f64, device_rate: f64, push_frames: usize, device_ppm: f64) -> Self {
            Self {
                ring: Ring::new(),
                playout: Playout::new(source_rate, device_rate, TICKS_PER_SECOND).unwrap(),
                source_rate,
                push_frames,
                next_push: 0.0,
                next_render: 0.0013,
                produced: 0,
                rendered: Vec::new(),
                render_period: RENDER_FRAMES as f64 / (device_rate * (1.0 + device_ppm * 1e-6)),
            }
        }

        fn run(&mut self, seconds: f64) {
            let end = self.next_render + seconds;
            while self.next_render < end {
                if self.next_push <= self.next_render {
                    let (rate, start) = (self.source_rate, self.produced);
                    self.ring.push_with(self.push_frames, (self.next_push * TICKS_PER_SECOND) as u64, |f, _| {
                        (0.5 * (2.0 * std::f64::consts::PI * TONE_HZ * (start + f) as f64 / rate).sin()) as f32
                    });
                    self.produced += self.push_frames;
                    self.next_push += self.push_frames as f64 / self.source_rate;
                } else {
                    let (mut left, mut right) = (vec![0.0; RENDER_FRAMES], vec![0.0; RENDER_FRAMES]);
                    let now = (self.next_render * TICKS_PER_SECOND) as u64;
                    self.playout.render(&self.ring, &mut [&mut left, &mut right], now);
                    assert_eq!(left, right);
                    self.rendered.extend_from_slice(&left);
                    self.next_render += self.render_period;
                }
            }
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
        for (source, device, push) in [(48000.0, 48000.0, 512), (48000.0, 44100.0, 512), (44100.0, 48000.0, 470)] {
            let mut sim = Sim::new(source, device, push, 0.0);
            sim.run(5.0);
            let steady = &sim.rendered[sim.first_sound() + 4096..];
            let peak = steady.iter().fold(0.0f32, |p, s| p.max(s.abs()));
            assert!((peak - 0.5).abs() < 0.01, "{source} -> {device}: peak {peak}");
            let error = worst_recurrence_error(steady, device);
            assert!(error < 1e-3, "{source} -> {device}: recurrence error {error}");
            assert_eq!(sim.playout.health.report().underruns, 0, "{source} -> {device}");
        }
    }

    #[test]
    fn stays_silent_until_the_target_fill_is_reached() {
        let mut sim = Sim::new(48000.0, 44100.0, 470, 0.0);
        sim.run(1.0);
        let target = sim.playout.health.report().target_frames as usize;
        assert!(target > RENDER_FRAMES * 48 / 44 + 470, "target {target}");
        let first_output = sim.rendered.iter().position(|s| *s != 0.0).unwrap();
        assert!(first_output >= RENDER_FRAMES, "played after {first_output} frames");
        assert_eq!(first_output % RENDER_FRAMES, 0, "playback starts mid-render");
    }

    #[test]
    fn reprimes_after_the_producer_stalls() {
        let mut sim = Sim::new(48000.0, 48000.0, 512, 0.0);
        sim.run(1.0);
        sim.next_push += 0.1;
        sim.run(1.0);
        let report = sim.playout.health.report();
        assert_eq!(report.underruns, 1);
        let tail = &sim.rendered[sim.rendered.len() - 8192..];
        assert!(worst_recurrence_error(tail, 48000.0) < 1e-3);
    }
}
