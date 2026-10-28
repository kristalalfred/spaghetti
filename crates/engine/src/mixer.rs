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

    /// Claude: the gain for each of `frames` frames, or none when the buffer
    /// stays at unity and needs no scaling.
    fn gains(&mut self, frames: usize, target: f32) -> Option<impl Iterator<Item = f32> + use<>> {
        let from = self.gain;
        if frames == 0 || (from == 1.0 && target == 1.0) {
            return None;
        }
        self.gain = target;
        let step = (target - from) / frames as f32;
        Some((1..=frames).map(move |frame| {
            if frame == frames {
                target
            } else {
                from + step * frame as f32
            }
        }))
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
    fn interleaved_channels_of_a_frame_share_its_gain() {
        let mut fader = Fader::new(0.0);
        let mut buf = [1.0f32, -1.0, 1.0, -1.0];
        fader.interleaved(&mut buf, 2, 1.0);
        assert_eq!(buf, [0.5, -0.5, 1.0, -1.0]);
    }
}
