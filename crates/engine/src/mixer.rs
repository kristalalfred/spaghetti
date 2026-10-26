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

/// Apply volume scaling to a buffer in-place, clamping to [-1.0, 1.0].
pub fn apply_volume(buffer: &mut [f32], volume: f32) {
    if (volume - 1.0).abs() < f32::EPSILON {
        return;
    }
    for sample in buffer.iter_mut() {
        *sample = (*sample * volume).clamp(-1.0, 1.0);
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
    fn apply_volume_unity_is_noop() {
        let mut buf = [0.5f32, -0.3];
        let original = buf;
        apply_volume(&mut buf, 1.0);
        assert_eq!(buf, original);
    }

    #[test]
    fn apply_volume_scales() {
        let mut buf = [1.0f32, -0.5];
        apply_volume(&mut buf, 0.5);
        assert!((buf[0] - 0.5).abs() < 1e-6);
        assert!((buf[1] - -0.25).abs() < 1e-6);
    }

    #[test]
    fn apply_volume_clamps_when_over_unity() {
        let mut buf = [0.9f32, -0.9];
        apply_volume(&mut buf, 2.0);
        assert_eq!(buf[0], 1.0);
        assert_eq!(buf[1], -1.0);
    }
}
