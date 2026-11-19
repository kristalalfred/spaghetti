use coreaudio_sys::AudioBuffer;

/// Claude: where the device's first two output channels live in its buffers,
/// which may interleave them in one buffer or hold one each. A mono device
/// gets both channels averaged.
pub(crate) struct StereoOut {
    left: Channel,
    right: Channel,
    pub(crate) frames: usize,
}

#[derive(Clone, Copy)]
struct Channel {
    data: *mut f32,
    stride: usize,
}

impl StereoOut {
    pub(crate) fn new(buffers: &[AudioBuffer]) -> Option<Self> {
        let mut channels = buffers
            .iter()
            .filter(|buffer| !buffer.mData.is_null() && buffer.mNumberChannels > 0)
            .flat_map(|buffer| {
                let stride = buffer.mNumberChannels as usize;
                let data = buffer.mData as *mut f32;
                let frames = buffer.mDataByteSize as usize / (stride * std::mem::size_of::<f32>());
                (0..stride).map(move |channel| {
                    let data = unsafe { data.add(channel) };
                    (Channel { data, stride }, frames)
                })
            });
        let (left, left_frames) = channels.next()?;
        let (right, right_frames) = channels.next().unwrap_or((left, left_frames));
        Some(Self {
            left,
            right,
            frames: left_frames.min(right_frames),
        })
    }

    /// # Safety
    ///
    /// Claude: `frame` must be below `self.frames`.
    pub(crate) unsafe fn add(&self, frame: usize, left: f32, right: f32) {
        unsafe {
            if self.left.data == self.right.data {
                *self.left.data.add(frame * self.left.stride) += (left + right) * 0.5;
            } else {
                *self.left.data.add(frame * self.left.stride) += left;
                *self.right.data.add(frame * self.right.stride) += right;
            }
        }
    }

    pub(crate) fn clamp(&self) {
        for channel in [self.left, self.right] {
            for frame in 0..self.frames {
                let sample = unsafe { &mut *channel.data.add(frame * channel.stride) };
                *sample = sample.clamp(-1.0, 1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(samples: &mut [f32], channels: u32) -> AudioBuffer {
        AudioBuffer {
            mNumberChannels: channels,
            mDataByteSize: std::mem::size_of_val(samples) as u32,
            mData: samples.as_mut_ptr().cast(),
        }
    }

    #[test]
    fn interleaved_stereo_gets_left_then_right() {
        let mut samples = [0.0f32; 4];
        let out = StereoOut::new(&[buffer(&mut samples, 2)]).unwrap();
        assert_eq!(out.frames, 2);
        unsafe {
            out.add(0, 0.25, -0.25);
            out.add(1, 0.5, -0.5);
            out.add(1, 0.75, -0.75);
        }
        out.clamp();
        assert_eq!(samples, [0.25, -0.25, 1.0, -1.0]);
    }

    #[test]
    fn separate_buffers_each_get_a_channel_and_extra_channels_stay_silent() {
        let mut left = [0.0f32; 2];
        let mut right = [0.0f32; 2];
        let mut rear = [0.0f32; 2];
        let buffers = [
            buffer(&mut left, 1),
            buffer(&mut right, 1),
            buffer(&mut rear, 1),
        ];
        let out = StereoOut::new(&buffers).unwrap();
        unsafe { out.add(1, 0.5, -0.5) };
        assert_eq!((left, right, rear), ([0.0, 0.5], [0.0, -0.5], [0.0, 0.0]));
    }

    #[test]
    fn a_mono_device_gets_the_average() {
        let mut mono = [0.0f32; 1];
        let out = StereoOut::new(&[buffer(&mut mono, 1)]).unwrap();
        unsafe { out.add(0, 0.5, 0.25) };
        assert_eq!(mono, [0.375]);
    }
}
