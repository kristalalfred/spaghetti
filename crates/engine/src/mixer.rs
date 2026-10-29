use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use coreaudio_sys::{AudioGetCurrentHostTime, AudioGetHostClockFrequency};

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
pub const START_LIMIT: Duration = Duration::from_millis(250);
pub const FADE_LIMIT: Duration = Duration::from_millis(250);
/// Claude: an output that has not cycled for this long is not playing.
pub const IDLE: Duration = Duration::from_millis(100);

/// Claude: counts the cycles of an IOProc or render callback and when the
/// latest ran, so the control side can wait for a change to take effect there.
pub struct Cycles {
    count: AtomicU64,
    at: AtomicU64,
}

impl Cycles {
    pub fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            at: AtomicU64::new(0),
        }
    }

    /// Claude: real-time safe, called once a cycle's output is written. `now`
    /// is the cycle's host time.
    pub fn tick(&self, now: u64) {
        self.at.store(now, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Release);
    }

    /// Claude: whether a cycle ran within `within`.
    pub fn running(&self, within: Duration) -> bool {
        let ticks = within.as_secs_f64() * unsafe { AudioGetHostClockFrequency() };
        let now = unsafe { AudioGetCurrentHostTime() };
        let at = self.at.load(Ordering::Relaxed);
        at != 0 && (now.saturating_sub(at) as f64) < ticks
    }

    /// Claude: waits until `cycles` more cycles have run, or `limit` passed.
    /// Returns whether they ran.
    pub fn wait(&self, cycles: u64, limit: Duration) -> bool {
        let target = self.count.load(Ordering::Acquire) + cycles;
        let deadline = Instant::now() + limit;
        while self.count.load(Ordering::Acquire) < target {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }
}

/// Claude: a route's volume, applied to consecutive buffers. A change ramps
/// linearly across the next buffer, because stepping the gain between two
/// buffers clicks. Scaled samples are clamped to [-1.0, 1.0].
pub struct Fader {
    gain: f32,
}

impl Fader {
    pub fn new(gain: f32) -> Self {
        Self { gain }
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

    /// Claude: the gain for each of the next `frames` frames.
    pub fn ramp(&mut self, frames: usize, target: f32) -> impl Iterator<Item = f32> + use<> {
        let from = self.gain;
        if frames > 0 {
            self.gain = target;
        }
        let step = (target - from) / frames.max(1) as f32;
        (1..=frames).map(move |frame| {
            if frame == frames {
                target
            } else {
                from + step * frame as f32
            }
        })
    }

    /// Claude: the gain for each of `frames` frames, or none when the buffer
    /// stays at unity and needs no scaling.
    fn gains(&mut self, frames: usize, target: f32) -> Option<impl Iterator<Item = f32> + use<>> {
        if frames == 0 || (self.gain == 1.0 && target == 1.0) {
            return None;
        }
        Some(self.ramp(frames, target))
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
        Fader::new(1.0).planar(&mut [left, right], 1.0);
        assert_eq!(planes, original);
    }

    #[test]
    fn a_steady_fader_scales_and_clamps() {
        let mut buf = [0.5f32, -0.5, 0.9, -0.9];
        Fader::new(0.5).interleaved(&mut buf, 2, 0.5);
        assert_eq!(buf, [0.25, -0.25, 0.45, -0.45]);
        Fader::new(2.0).interleaved(&mut buf, 2, 2.0);
        assert_eq!(buf, [0.5, -0.5, 0.9, -0.9]);
        let mut loud = [0.9f32, -0.9];
        Fader::new(2.0).interleaved(&mut loud, 2, 2.0);
        assert_eq!(loud, [1.0, -1.0]);
    }

    #[test]
    fn a_volume_change_ramps_across_one_buffer_then_holds() {
        let mut fader = Fader::new(1.0);
        let mut left = vec![1.0f32; 4];
        let mut right = vec![1.0f32; 4];
        fader.planar(&mut [&mut left, &mut right], 0.0);
        assert_eq!(left, [0.75, 0.5, 0.25, 0.0]);
        assert_eq!(right, left);

        let mut next = vec![1.0f32; 4];
        fader.planar(&mut [&mut next], 0.0);
        assert_eq!(next, [0.0; 4]);
    }

    #[test]
    fn a_ramp_yields_every_frame_even_at_unity() {
        let mut fader = Fader::new(1.0);
        assert_eq!(fader.ramp(2, 1.0).collect::<Vec<_>>(), [1.0, 1.0]);
        assert_eq!(fader.ramp(2, 0.0).collect::<Vec<_>>(), [0.5, 0.0]);
        assert_eq!(fader.ramp(0, 1.0).count(), 0);
        assert_eq!(fader.ramp(1, 0.0).collect::<Vec<_>>(), [0.0]);
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
                    cycles.tick(unsafe { AudioGetCurrentHostTime() });
                }
            }
        });
        assert!(cycles.wait(2, Duration::from_secs(1)));
        ticking.join().unwrap();
        assert!(cycles.running(Duration::from_secs(1)));
    }

    #[test]
    fn interleaved_channels_of_a_frame_share_its_gain() {
        let mut fader = Fader::new(0.0);
        let mut buf = [1.0f32, -1.0, 1.0, -1.0];
        fader.interleaved(&mut buf, 2, 1.0);
        assert_eq!(buf, [0.5, -0.5, 1.0, -1.0]);
    }
}
