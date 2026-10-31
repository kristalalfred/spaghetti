use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::host_time;

/// Mix multiple audio sources with individual volumes into an output buffer.
/// All buffers are interleaved f32.
#[allow(dead_code)]
pub fn mix(inputs: &[&[f32]], volumes: &[f32], output: &mut [f32]) {
    output.fill(0.0);

    for (input, &vol) in inputs.iter().zip(volumes.iter()) {
        for (out_sample, &in_sample) in output.iter_mut().zip(input.iter()) {
            *out_sample += in_sample * vol;
        }
    }

    for sample in output.iter_mut() {
        *sample = sample.clamp(-1.0, 1.0);
    }
}

/// Claude: how long a new tap waits for its first cycles, and a route for its
/// fade-out, before going on regardless.
#[cfg(target_os = "macos")]
pub const START_LIMIT: Duration = Duration::from_millis(250);
pub const FADE_LIMIT: Duration = Duration::from_millis(250);
/// Claude: an output that has not cycled for this long is not playing.
pub const IDLE: Duration = Duration::from_millis(100);

/// Claude: counts the cycles of an IOProc or render callback and when the
/// latest ran, so the control side can wait for a change to take effect there.
pub struct Cycles {
    sequence: AtomicU64,
    rendered_ns: AtomicU64,
    at: AtomicU64,
}

struct Progress {
    cycles: u64,
    rendered_ns: u64,
}

impl Cycles {
    pub fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            rendered_ns: AtomicU64::new(0),
            at: AtomicU64::new(0),
        }
    }

    // Codex: one callback writes this counter. Odd sequences exclude a partial
    // tick from snapshots; readers retry through their deadline, never spin.
    pub fn tick(&self, now: u64, frames: usize, rate: f64) {
        let rendered_ns = (frames as f64 * 1e9 / rate) as u64;
        self.sequence.fetch_add(1, Ordering::SeqCst);
        self.rendered_ns.fetch_add(rendered_ns, Ordering::SeqCst);
        self.at.store(now, Ordering::Relaxed);
        self.sequence.fetch_add(1, Ordering::SeqCst);
    }

    fn progress(&self) -> Option<Progress> {
        let before = self.sequence.load(Ordering::SeqCst);
        if !before.is_multiple_of(2) {
            return None;
        }
        let rendered_ns = self.rendered_ns.load(Ordering::SeqCst);
        (self.sequence.load(Ordering::SeqCst) == before).then_some(Progress {
            cycles: before / 2,
            rendered_ns,
        })
    }

    /// Claude: whether a cycle ran within `within`.
    pub fn running(&self, within: Duration) -> bool {
        let ticks = within.as_secs_f64() * host_time::ticks_per_second();
        let now = host_time::now();
        let at = self.at.load(Ordering::Relaxed);
        at != 0 && (now.saturating_sub(at) as f64) < ticks
    }

    pub fn wait_fade(&self, queued_cycles: u64, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let before = self.sequence.load(Ordering::SeqCst) / 2;
        let Some(start) = self.wait_until(deadline, |p| p.cycles > before) else {
            return false;
        };
        let Some(faded) = self.wait_until(deadline, |p| {
            p.rendered_ns.saturating_sub(start.rendered_ns) >= RAMP.as_nanos() as u64
        }) else {
            return false;
        };
        self.wait_until(deadline, |p| p.cycles >= faded.cycles + queued_cycles)
            .is_some()
    }

    #[cfg(any(target_os = "macos", test))]
    pub fn wait(&self, cycles: u64, limit: Duration) -> bool {
        let target = self.sequence.load(Ordering::SeqCst) / 2 + cycles;
        self.wait_until(Instant::now() + limit, |p| p.cycles >= target)
            .is_some()
    }

    fn wait_until(
        &self,
        deadline: Instant,
        condition: impl Fn(&Progress) -> bool,
    ) -> Option<Progress> {
        loop {
            if let Some(progress) = self.progress().filter(&condition) {
                return Some(progress);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Claude: how long a volume change ramps. Stepping the gain clicks, and so
/// does a ramp shorter than a bass wave, whatever the buffer size.
pub const RAMP: Duration = Duration::from_millis(50);

/// Claude: a route's volume, applied to consecutive buffers. A change ramps
/// linearly over `RAMP`, across as many buffers as that takes. Scaled samples
/// are clamped to [-1.0, 1.0].
pub struct Fader {
    gain: f32,
    target: f32,
    step: f32,
    ramp_frames: f32,
}

impl Fader {
    pub fn new(gain: f32, sample_rate: f64) -> Self {
        Self {
            gain,
            target: gain,
            step: 0.0,
            ramp_frames: (RAMP.as_secs_f64() * sample_rate).max(1.0) as f32,
        }
    }

    /// Scale channels held in separate planes of equal length.
    pub fn planar(&mut self, planes: &mut [&mut [f32]], target: f32) {
        let frames = planes.iter().map(|plane| plane.len()).min().unwrap_or(0);
        let Some(gains) = self.gains(frames, target) else {
            return;
        };
        for (frame, gain) in gains.enumerate() {
            for plane in planes.iter_mut() {
                plane[frame] = (plane[frame] * gain).clamp(-1.0, 1.0);
            }
        }
    }

    /// Scale frames of `channels` interleaved samples.
    pub fn interleaved(&mut self, buffer: &mut [f32], channels: usize, target: f32) {
        let Some(gains) = self.gains(buffer.len() / channels, target) else {
            return;
        };
        for (frame, gain) in buffer.chunks_exact_mut(channels).zip(gains) {
            for sample in frame {
                *sample = (*sample * gain).clamp(-1.0, 1.0);
            }
        }
    }

    /// Claude: the gain for each of the next `frames` frames. A new target
    /// starts a ramp to it from wherever the gain is.
    pub fn ramp(&mut self, frames: usize, target: f32) -> impl Iterator<Item = f32> + use<> {
        if target != self.target {
            self.target = target;
            self.step = (target - self.gain) / self.ramp_frames;
        }
        let (from, step) = (self.gain, self.step);
        self.gain = short_of(from + step * frames as f32, step, target);
        if self.gain == target {
            self.step = 0.0;
        }
        (1..=frames).map(move |frame| short_of(from + step * frame as f32, step, target))
    }

    /// Claude: the gain for each of `frames` frames, or none when the buffer
    /// stays at unity and needs no scaling.
    fn gains(&mut self, frames: usize, target: f32) -> Option<impl Iterator<Item = f32> + use<>> {
        if frames == 0 || (self.gain == 1.0 && self.target == 1.0 && target == 1.0) {
            return None;
        }
        Some(self.ramp(frames, target))
    }
}

/// Claude: `gain`, unless a ramp by `step` has carried it past `target`.
fn short_of(gain: f32, step: f32, target: f32) -> f32 {
    if (step > 0.0 && gain > target) || (step < 0.0 && gain < target) {
        target
    } else {
        gain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_two_sources() {
        let a = [0.5f32, 0.5];
        let b = [0.3f32, -0.3];
        let mut out = [0.0f32; 2];

        mix(&[&a, &b], &[1.0, 1.0], &mut out);

        assert!((out[0] - 0.8).abs() < 1e-6);
        assert!((out[1] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn mix_clamps_output() {
        let a = [0.9f32];
        let b = [0.9f32];
        let mut out = [0.0f32; 1];

        mix(&[&a, &b], &[1.0, 1.0], &mut out);

        assert_eq!(out[0], 1.0);
    }

    #[test]
    fn a_fader_at_unity_leaves_samples_alone() {
        let mut planes = [vec![1.5f32, -0.3], vec![0.2, 0.4]];
        let original = planes.clone();
        let [left, right] = &mut planes;
        Fader::new(1.0, 48000.0).planar(&mut [left, right], 1.0);
        assert_eq!(planes, original);
    }

    #[test]
    fn a_steady_fader_scales_and_clamps() {
        let mut buf = [0.5f32, -0.5, 0.9, -0.9];
        Fader::new(0.5, 48000.0).interleaved(&mut buf, 2, 0.5);
        assert_eq!(buf, [0.25, -0.25, 0.45, -0.45]);
        Fader::new(2.0, 48000.0).interleaved(&mut buf, 2, 2.0);
        assert_eq!(buf, [0.5, -0.5, 0.9, -0.9]);
        let mut loud = [0.9f32, -0.9];
        Fader::new(2.0, 48000.0).interleaved(&mut loud, 2, 2.0);
        assert_eq!(loud, [1.0, -1.0]);
    }

    /// Claude: at this rate `RAMP` lasts 4 frames.
    const FOUR_FRAME_RAMP: f64 = 80.0;

    #[test]
    fn a_volume_change_ramps_over_its_length_across_buffers_then_holds() {
        let mut fader = Fader::new(1.0, FOUR_FRAME_RAMP);
        let mut left = vec![1.0f32; 3];
        let mut right = vec![1.0f32; 3];
        fader.planar(&mut [&mut left, &mut right], 0.0);
        assert_eq!(left, [0.75, 0.5, 0.25]);
        assert_eq!(right, left);

        let mut next = vec![1.0f32; 3];
        fader.planar(&mut [&mut next], 0.0);
        assert_eq!(next, [0.0; 3]);
    }

    #[test]
    fn a_new_target_mid_ramp_ramps_from_where_the_gain_is() {
        let mut fader = Fader::new(0.0, FOUR_FRAME_RAMP);
        assert_eq!(fader.ramp(2, 1.0).collect::<Vec<_>>(), [0.25, 0.5]);
        assert_eq!(fader.ramp(2, 0.0).collect::<Vec<_>>(), [0.375, 0.25]);
        assert_eq!(fader.ramp(3, 0.0).collect::<Vec<_>>(), [0.125, 0.0, 0.0]);
    }

    #[test]
    fn a_ramp_yields_every_frame_even_at_unity() {
        let mut fader = Fader::new(1.0, FOUR_FRAME_RAMP);
        assert_eq!(fader.ramp(2, 1.0).collect::<Vec<_>>(), [1.0, 1.0]);
        assert_eq!(fader.ramp(0, 1.0).count(), 0);
    }

    #[test]
    fn waiting_for_cycles_returns_once_they_run_or_gives_up() {
        let cycles = std::sync::Arc::new(Cycles::new());
        assert!(!cycles.running(Duration::from_secs(1)));
        assert!(!cycles.wait(1, Duration::from_millis(20)));
        let ticking = std::thread::spawn({
            let cycles = std::sync::Arc::clone(&cycles);
            move || {
                for _ in 0..3 {
                    std::thread::sleep(Duration::from_millis(5));
                    cycles.tick(host_time::now(), 352, 44100.0);
                }
            }
        });
        assert!(cycles.wait(2, Duration::from_secs(1)));
        ticking.join().unwrap();
        assert!(cycles.running(Duration::from_secs(1)));
    }

    #[test]
    fn a_fade_waits_for_rendered_audio_after_a_stall_longer_than_idle() {
        use std::sync::{Arc, atomic::AtomicUsize};
        let cycles = Arc::new(Cycles::new());
        cycles.tick(host_time::now(), 352, 44100.0);
        let rendered = Arc::new(AtomicUsize::new(0));
        let ticking = std::thread::spawn({
            let cycles = Arc::clone(&cycles);
            let rendered = Arc::clone(&rendered);
            move || {
                std::thread::sleep(IDLE + Duration::from_millis(10));
                for _ in 0..11 {
                    std::thread::sleep(Duration::from_millis(8));
                    rendered.fetch_add(352, Ordering::Release);
                    cycles.tick(host_time::now(), 352, 44100.0);
                }
            }
        });
        let finished = cycles.wait_fade(3, FADE_LIMIT);
        let frames = rendered.load(Ordering::Acquire);
        ticking.join().unwrap();
        assert!(finished);
        assert!(frames >= 11 * 352, "finished after only {frames} frames");
    }

    #[test]
    fn a_fade_counts_rendered_duration_across_different_rates_and_sizes() {
        use std::sync::{Arc, atomic::AtomicUsize};
        let cycles = Arc::new(Cycles::new());
        let rendered = Arc::new(AtomicUsize::new(0));
        let ticking = std::thread::spawn({
            let cycles = Arc::clone(&cycles);
            let rendered = Arc::clone(&rendered);
            move || {
                for (frames, rate) in [
                    (352, 44100.0),
                    (480, 48000.0),
                    (960, 96000.0),
                    (441, 44100.0),
                    (960, 96000.0),
                    (480, 48000.0),
                    (352, 44100.0),
                    (352, 44100.0),
                ] {
                    std::thread::sleep(Duration::from_millis(8));
                    rendered.fetch_add(1, Ordering::Release);
                    cycles.tick(host_time::now(), frames, rate);
                }
            }
        });
        let finished = cycles.wait_fade(2, FADE_LIMIT);
        let rendered = rendered.load(Ordering::Acquire);
        ticking.join().unwrap();
        assert!(finished);
        assert_eq!(rendered, 8);
    }

    #[test]
    fn a_fade_that_never_resumes_has_one_bounded_deadline() {
        let cycles = Cycles::new();
        cycles.tick(host_time::now(), 352, 44100.0);
        let started = Instant::now();
        assert!(!cycles.wait_fade(3, FADE_LIMIT));
        assert!(started.elapsed() < FADE_LIMIT + Duration::from_millis(40));
    }

    #[test]
    fn interleaved_channels_of_a_frame_share_its_gain() {
        let mut fader = Fader::new(0.0, FOUR_FRAME_RAMP);
        let mut buf = [1.0f32, -1.0, 1.0, -1.0];
        fader.interleaved(&mut buf, 2, 1.0);
        assert_eq!(buf, [0.25, -0.25, 0.5, -0.5]);
    }
}
