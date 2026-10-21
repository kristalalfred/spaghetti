use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const CHANNELS: usize = 2;
const CAPACITY_FRAMES: usize = 16384;
const MASK: usize = CAPACITY_FRAMES - 1;

/// Claude: single-producer single-consumer FIFO of interleaved stereo f32 frames.
/// `head` and `tail` count frames and are only advanced by the producer and the
/// consumer respectively.
pub struct Ring {
    samples: Box<[UnsafeCell<f32>]>,
    head: AtomicUsize,
    tail: AtomicUsize,
}

// Claude: frames between `tail` and `head` belong to the consumer and the rest to
// the producer, so the two sides never touch the same sample at the same time.
unsafe impl Sync for Ring {}

impl Ring {
    pub fn new() -> Self {
        Self {
            samples: (0..CAPACITY_FRAMES * CHANNELS)
                .map(|_| UnsafeCell::new(0.0))
                .collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    pub fn available(&self) -> usize {
        let tail = self.tail.load(Ordering::Acquire);
        self.head.load(Ordering::Acquire).wrapping_sub(tail)
    }

    /// Claude: producer only. Appends `frames` frames, or none when they do not fit.
    pub fn push_with(&self, frames: usize, mut sample: impl FnMut(usize, usize) -> f32) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if CAPACITY_FRAMES - head.wrapping_sub(tail) < frames {
            return false;
        }
        for frame in 0..frames {
            let base = (head.wrapping_add(frame) & MASK) * CHANNELS;
            for ch in 0..CHANNELS {
                unsafe { *self.samples[base + ch].get() = sample(frame, ch) };
            }
        }
        self.head.store(head.wrapping_add(frames), Ordering::Release);
        true
    }

    /// Claude: consumer only. Fills `dst` with up to `dst.len() / CHANNELS` frames
    /// and returns how many it read.
    pub fn pop_into(&self, dst: &mut [f32]) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let frames = head.wrapping_sub(tail).min(dst.len() / CHANNELS);
        for frame in 0..frames {
            let base = (tail.wrapping_add(frame) & MASK) * CHANNELS;
            for ch in 0..CHANNELS {
                dst[frame * CHANNELS + ch] = unsafe { *self.samples[base + ch].get() };
            }
        }
        self.tail.store(tail.wrapping_add(frames), Ordering::Release);
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_counting(ring: &Ring, start: usize, frames: usize) -> bool {
        ring.push_with(frames, |f, ch| ((start + f) * CHANNELS + ch) as f32)
    }

    #[test]
    fn frames_come_out_in_order_across_the_wrap() {
        let ring = Ring::new();
        let mut written = 0;
        let mut read = 0;
        let mut dst = vec![0.0; 1000 * CHANNELS];
        for _ in 0..100 {
            assert!(push_counting(&ring, written, 700));
            written += 700;
            let frames = ring.pop_into(&mut dst);
            for (i, sample) in dst[..frames * CHANNELS].iter().enumerate() {
                assert_eq!(*sample, (read * CHANNELS + i) as f32);
            }
            read += frames;
        }
        assert_eq!(written, read + ring.available());
        assert!(written > CAPACITY_FRAMES * 3);
    }

    #[test]
    fn refuses_pushes_that_do_not_fit() {
        let ring = Ring::new();
        assert!(push_counting(&ring, 0, CAPACITY_FRAMES - 10));
        assert!(!push_counting(&ring, 0, 11));
        assert!(push_counting(&ring, 0, 10));
        assert_eq!(ring.available(), CAPACITY_FRAMES);
    }
}
