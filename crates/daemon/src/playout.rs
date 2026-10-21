use rubato::audioadapter_buffers::direct::{InterleavedSlice, SequentialSliceOfSlices};
use rubato::{
    Async, FixedAsync, Indexing, Resampler, Resizable, SincInterpolationParameters, WindowFunction,
};

use crate::ring::{CHANNELS, Ring};

pub const MAX_RENDER_FRAMES: usize = 4096;
const MAX_RATIO_RELATIVE: f64 = 1.01;
const SINC_LEN: usize = 128;

/// Claude: the output side of a route. Resamples source-rate frames from the
/// ring to exactly the number of device-rate frames each render asks for.
pub struct Playout {
    resampler: Async<f32>,
    input: Vec<f32>,
    indexing: Indexing,
}

impl Playout {
    pub fn new(source_rate: f64, device_rate: f64) -> Result<Self, String> {
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
        })
    }

    pub fn delay_frames(&self) -> usize {
        self.resampler.output_delay()
    }

    /// Claude: both channel slices must have the same length.
    pub fn render(&mut self, ring: &Ring, out: &mut [&mut [f32]; CHANNELS]) {
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
        let got = ring.pop_into(&mut self.input[..need * CHANNELS]);
        self.indexing.partial_len = (got < need).then_some(got);
        if !self.resample(need, out) {
            silence(out);
        }
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

    struct Tone {
        rate: f64,
        frame: usize,
    }

    impl Tone {
        fn push(&mut self, ring: &Ring, frames: usize) {
            let (rate, start) = (self.rate, self.frame);
            ring.push_with(frames, |f, _| {
                (0.5 * (2.0 * std::f64::consts::PI * TONE_HZ * (start + f) as f64 / rate).sin()) as f32
            });
            self.frame += frames;
        }
    }

    fn render_tone(source_rate: f64, device_rate: f64, chunks: usize) -> Vec<f32> {
        let ring = Ring::new();
        let mut tone = Tone { rate: source_rate, frame: 0 };
        let mut playout = Playout::new(source_rate, device_rate).unwrap();
        let mut rendered = Vec::new();
        for _ in 0..chunks {
            while ring.available() < 2048 {
                tone.push(&ring, 512);
            }
            let (mut left, mut right) = (vec![0.0; 512], vec![0.0; 512]);
            playout.render(&ring, &mut [&mut left, &mut right]);
            assert_eq!(left, right);
            rendered.extend_from_slice(&left);
        }
        rendered
    }

    fn worst_recurrence_error(samples: &[f32], rate: f64) -> f64 {
        let k = 2.0 * (2.0 * std::f64::consts::PI * TONE_HZ / rate).cos();
        samples
            .windows(3)
            .map(|w| (w[2] as f64 - (k * w[1] as f64 - w[0] as f64)).abs())
            .fold(0.0, f64::max)
    }

    #[test]
    fn resamples_a_continuous_tone_across_render_calls() {
        for (source, device) in [(48000.0, 48000.0), (48000.0, 44100.0), (44100.0, 48000.0)] {
            let rendered = render_tone(source, device, 200);
            let steady = &rendered[4096..];
            let peak = steady.iter().fold(0.0f32, |p, s| p.max(s.abs()));
            assert!((peak - 0.5).abs() < 0.01, "{source} -> {device}: peak {peak}");
            let error = worst_recurrence_error(steady, device);
            assert!(error < 1e-3, "{source} -> {device}: recurrence error {error}");
        }
    }
}
