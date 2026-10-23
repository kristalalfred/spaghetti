use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use coreaudio_sys::AudioGetCurrentHostTime;

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
    /// Claude: the head after the latest push in the high half and that push's
    /// host time in the low half, both truncated to u32 so one atomic holds them.
    last_push: AtomicU64,
    largest_push: AtomicUsize,
    overflows: AtomicU32,
}

/// Claude: what the consumer can see of the latest push.
pub struct LastPush {
    pub buffered: usize,
    pub at: u32,
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
            last_push: AtomicU64::new(0),
            largest_push: AtomicUsize::new(0),
            overflows: AtomicU32::new(0),
        }
    }

    pub fn available(&self) -> usize {
        let tail = self.tail.load(Ordering::Acquire);
        self.head.load(Ordering::Acquire).wrapping_sub(tail)
    }

    pub fn largest_push(&self) -> usize {
        self.largest_push.load(Ordering::Relaxed)
    }

    pub fn overflows(&self) -> u32 {
        self.overflows.load(Ordering::Relaxed)
    }

    /// Claude: producer only. Appends `frames` frames stamped with host time `now`,
    /// or drops them all when they do not fit.
    pub fn push_with(
        &self,
        frames: usize,
        now: u64,
        mut sample: impl FnMut(usize, usize) -> f32,
    ) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        self.largest_push.fetch_max(frames, Ordering::Relaxed);
        if CAPACITY_FRAMES - head.wrapping_sub(tail) < frames {
            self.overflows.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        for frame in 0..frames {
            let base = (head.wrapping_add(frame) & MASK) * CHANNELS;
            for ch in 0..CHANNELS {
                unsafe { *self.samples[base + ch].get() = sample(frame, ch) };
            }
        }
        let new_head = head.wrapping_add(frames);
        self.last_push
            .store(((new_head as u64) << 32) | (now & 0xffff_ffff), Ordering::Release);
        self.head.store(new_head, Ordering::Release);
        true
    }

    /// Claude: consumer only. `last_push` is stored before `head`, so the frames it
    /// reports never trail the consumer's own tail.
    pub fn last_push(&self) -> LastPush {
        let packed = self.last_push.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Relaxed) as u32;
        LastPush {
            buffered: ((packed >> 32) as u32).wrapping_sub(tail) as usize,
            at: packed as u32,
        }
    }

    /// Claude: consumer only. When the oldest frame in the ring was captured, from
    /// the host time of the latest push and the frames buffered since. Stamping
    /// packets with it makes the receiver release them at the source's cadence,
    /// not the encoder's.
    pub fn captured_at(&self, sample_rate: f64, ticks_per_second: f64) -> Instant {
        let last = self.last_push();
        let now = Instant::now();
        let since_push = (unsafe { AudioGetCurrentHostTime() } as u32).wrapping_sub(last.at) as f64
            / ticks_per_second;
        let age = since_push + last.buffered as f64 / sample_rate;
        now.checked_sub(Duration::from_secs_f64(age)).unwrap_or(now)
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

    /// Claude: consumer only.
    pub fn discard(&self, frames: usize) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let frames = head.wrapping_sub(tail).min(frames);
        self.tail.store(tail.wrapping_add(frames), Ordering::Release);
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_counting(ring: &Ring, start: usize, frames: usize) -> bool {
        ring.push_with(frames, 0, |f, ch| ((start + f) * CHANNELS + ch) as f32)
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

    #[test]
    fn discard_drops_the_oldest_frames() {
        let ring = Ring::new();
        push_counting(&ring, 0, 10);
        assert_eq!(ring.discard(4), 4);
        let mut dst = [0.0; CHANNELS];
        ring.pop_into(&mut dst);
        assert_eq!(dst[0], (4 * CHANNELS) as f32);
        assert_eq!(ring.discard(100), 5);
        assert_eq!(ring.available(), 0);
    }

    #[test]
    fn reports_frames_buffered_up_to_the_latest_push() {
        let ring = Ring::new();
        assert!(ring.push_with(100, 7, |_, _| 0.0));
        assert!(ring.push_with(50, (1u64 << 32) + 9, |_, _| 0.0));
        let mut dst = vec![0.0; 30 * CHANNELS];
        ring.pop_into(&mut dst);
        let last = ring.last_push();
        assert_eq!((last.buffered, last.at), (120, 9));
        assert_eq!(ring.largest_push(), 100);
    }

    #[test]
    fn counts_overflows() {
        let ring = Ring::new();
        assert!(push_counting(&ring, 0, CAPACITY_FRAMES));
        assert!(!push_counting(&ring, 0, 1));
        assert_eq!(ring.overflows(), 1);
    }
}
