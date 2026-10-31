use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence};
use std::time::{Duration, Instant};

use crate::host_time;

pub const CHANNELS: usize = 2;
/// Claude: 2.7 s at 48 kHz. At the longest span a network source reaches, a
/// route's fill may run up to two spans over what it renders before it resyncs.
pub const CAPACITY_FRAMES: usize = 1 << 17;
/// Claude: a ring keeps a record of each push it holds, up to one per this
/// many frames of its capacity on average.
const FRAMES_PER_SEGMENT: usize = 16;
/// Claude: a measured push dated this close to where the previous one ended
/// continues it. Further off, audio is missing or repeated between the two.
const RUN_TOLERANCE_SECONDS: f64 = 0.001;
/// Claude: how often the consumer reads a publication the producer is in the
/// middle of rewriting before it keeps the one it read last.
const READ_ATTEMPTS: usize = 8;
const NO_TIME: u64 = u64::MAX;

/// Claude: when a push's first frame was captured, as far as its source knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Captured {
    /// Claude: the source does not date its frames.
    Unknown,
    /// Claude: the host time read as the frame was captured. A push dated off
    /// where the previous one ended starts a new run.
    Measured(u64),
    /// Claude: the host time estimated through another clock, whose estimate
    /// may move between pushes. The source marks its own discontinuities.
    Estimated(u64),
}

/// Claude: what the producer publishes with each push, read by the consumer
/// as a whole. Positions count frames since the ring began, segments count
/// pushes, and runs count runs, all in 64 bits: at most one each per frame,
/// they do not wrap in centuries, and they compare wrapping where they do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Published {
    head: u64,
    /// Claude: the segment the next push records.
    pushed: u64,
    /// Claude: the newest run, where its first frame went and its first segment.
    run: u64,
    run_start: u64,
    run_segment: u64,
}

impl Published {
    fn to_words(self) -> [u64; 5] {
        [
            self.head,
            self.pushed,
            self.run,
            self.run_start,
            self.run_segment,
        ]
    }

    fn from_words(words: [u64; 5]) -> Self {
        let [head, pushed, run, run_start, run_segment] = words;
        Self {
            head,
            pushed,
            run,
            run_start,
            run_segment,
        }
    }
}

/// Claude: a `Published` the producer rewrites and the consumer reads whole,
/// behind a sequence count that is odd while a rewrite is under way.
struct Publication {
    sequence: AtomicU64,
    words: [AtomicU64; 5],
}

impl Publication {
    fn new(published: Published) -> Self {
        Self {
            sequence: AtomicU64::new(0),
            words: published.to_words().map(AtomicU64::new),
        }
    }

    /// Claude: producer only. What it published last.
    fn latest(&self) -> Published {
        Published::from_words(std::array::from_fn(|i| {
            self.words[i].load(Ordering::Relaxed)
        }))
    }

    /// Claude: producer only.
    fn write(&self, published: Published) {
        let sequence = self.sequence.load(Ordering::Relaxed);
        self.sequence
            .store(sequence.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (word, value) in self.words.iter().zip(published.to_words()) {
            word.store(value, Ordering::Relaxed);
        }
        self.sequence
            .store(sequence.wrapping_add(2), Ordering::Release);
    }

    /// Claude: consumer only. None while a rewrite is under way.
    fn read(&self) -> Option<Published> {
        let before = self.sequence.load(Ordering::Acquire);
        if before % 2 == 1 {
            return None;
        }
        let words = std::array::from_fn(|i| self.words[i].load(Ordering::Relaxed));
        fence(Ordering::Acquire);
        (self.sequence.load(Ordering::Relaxed) == before).then(|| Published::from_words(words))
    }
}

/// Claude: the record of one push: where its frames went, its run, when its
/// first frame was captured or `NO_TIME`, and when it arrived. The producer
/// writes it before publishing the push and leaves it alone until the consumer
/// is past it.
struct Segment([AtomicU64; 5]);

#[derive(Clone, Copy)]
struct Push {
    start: u64,
    len: u64,
    run: u64,
    captured: u64,
    arrival: u64,
}

impl Segment {
    fn new() -> Self {
        Self(std::array::from_fn(|_| AtomicU64::new(0)))
    }

    fn write(&self, push: Push) {
        for (word, value) in
            self.0
                .iter()
                .zip([push.start, push.len, push.run, push.captured, push.arrival])
        {
            word.store(value, Ordering::Relaxed);
        }
    }

    fn read(&self) -> Push {
        let [start, len, run, captured, arrival] =
            std::array::from_fn(|i| self.0[i].load(Ordering::Relaxed));
        Push {
            start,
            len,
            run,
            captured,
            arrival,
        }
    }
}

/// Claude: how many frames `position` lies ahead of `from`, negative behind.
fn ahead(position: u64, from: u64) -> i64 {
    position.wrapping_sub(from) as i64
}

/// Claude: single-producer single-consumer FIFO of interleaved stereo f32 frames
/// at one sample rate. The producer advances the head and the consumer the tail.
///
/// Claude: each push keeps a record of its own, so every frame is dated by the
/// push it came in: its capture time when the source dates its frames, else its
/// arrival. The frames come in runs. The first push starts one, and a run ends
/// where audio went missing: a push the ring had no room for, a measured push
/// dated off where the previous one ended, or a discontinuity the producer
/// marks. The producer publishes each push, its record and its run together
/// once its frames are written. The consumer reads only a run it took
/// (`take_discontinuity`), which drops what it has not read of the earlier runs.
pub struct Ring {
    samples: Box<[UnsafeCell<f32>]>,
    segments: Box<[Segment]>,
    tail: AtomicU64,
    /// Claude: the segment of the frame at the tail, or of the last frame read
    /// when the consumer has read all its run holds. Written by the consumer,
    /// it keeps that segment and all after it from the producer.
    cursor: AtomicU64,
    publication: Publication,
    /// Claude: consumer only. The latest publication it read whole.
    seen: UnsafeCell<Published>,
    /// Claude: consumer only. The run it took last.
    taken: AtomicU64,
    /// Claude: consumer only. One past the last segment it found of that run.
    reach: AtomicU64,
    /// Claude: consumer only. Whether it took a run yet.
    reading: AtomicBool,
    /// Claude: producer only. Where the latest measured push ended, or `NO_TIME`.
    next_capture: AtomicU64,
    /// Claude: producer only. The next push starts a run; set until the first.
    split: AtomicBool,
    /// Claude: producer only. Whether a run was started yet.
    started: AtomicBool,
    capacity: u64,
    mask: usize,
    segment_mask: usize,
    ticks_per_frame: f64,
    tolerance_ticks: f64,
    overflows: AtomicU32,
    discontinuities: AtomicU32,
}

/// Claude: what the consumer can see of the latest push.
pub struct LastPush {
    pub buffered: usize,
    /// Claude: the host time it arrived.
    pub at: u64,
}

// Claude: frames between the tail and the published head, and segments from
// the cursor to the published count, belong to the consumer and the rest to the
// producer, so the two sides never touch the same sample or record at the same
// time. `seen` is the consumer's alone.
unsafe impl Sync for Ring {}

impl Ring {
    pub fn new(sample_rate: f64) -> Self {
        Self::starting_at(sample_rate, 0, 0, CAPACITY_FRAMES)
    }

    /// Claude: a ring of `capacity` frames, a power of two, for tests that
    /// make many.
    #[cfg(test)]
    pub fn with_capacity(sample_rate: f64, capacity: usize) -> Self {
        Self::starting_at(sample_rate, 0, 0, capacity)
    }

    /// Claude: a ring whose positions, segments and runs start from `position`
    /// and `run`.
    fn starting_at(sample_rate: f64, position: u64, run: u64, capacity: usize) -> Self {
        assert!(
            capacity.is_power_of_two() && capacity >= FRAMES_PER_SEGMENT,
            "{capacity} frames is not a power of two"
        );
        let ticks_per_second = host_time::ticks_per_second();
        let published = Published {
            head: position,
            pushed: position,
            run,
            run_start: position,
            run_segment: position,
        };
        let segments = capacity / FRAMES_PER_SEGMENT;
        let samples = Box::into_raw(vec![0.0f32; capacity * CHANNELS].into_boxed_slice());
        Self {
            // Claude: `UnsafeCell<f32>` has the layout of `f32`.
            samples: unsafe { Box::from_raw(samples as *mut [UnsafeCell<f32>]) },
            segments: (0..segments).map(|_| Segment::new()).collect(),
            tail: AtomicU64::new(position),
            cursor: AtomicU64::new(position),
            publication: Publication::new(published),
            seen: UnsafeCell::new(published),
            taken: AtomicU64::new(run),
            reach: AtomicU64::new(position),
            reading: AtomicBool::new(false),
            next_capture: AtomicU64::new(NO_TIME),
            split: AtomicBool::new(true),
            started: AtomicBool::new(false),
            capacity: capacity as u64,
            mask: capacity - 1,
            segment_mask: segments - 1,
            ticks_per_frame: ticks_per_second / sample_rate,
            tolerance_ticks: RUN_TOLERANCE_SECONDS * ticks_per_second,
            overflows: AtomicU32::new(0),
            discontinuities: AtomicU32::new(0),
        }
    }

    pub fn overflows(&self) -> u32 {
        self.overflows.load(Ordering::Relaxed)
    }

    /// Claude: how many runs the producer started after the first.
    pub fn discontinuities(&self) -> u32 {
        self.discontinuities.load(Ordering::Relaxed)
    }

    fn segment(&self, index: u64) -> &Segment {
        &self.segments[index as usize & self.segment_mask]
    }

    /// Claude: producer only. Appends `frames` frames that arrived at host time
    /// `now` and whose first frame was `captured`, or drops them all when they
    /// or their record do not fit, which ends the run.
    pub fn push_with(
        &self,
        frames: usize,
        now: u64,
        captured: Captured,
        mut sample: impl FnMut(usize, usize) -> f32,
    ) -> bool {
        if frames == 0 {
            return true;
        }
        let latest = self.publication.latest();
        let head = latest.head;
        let tail = self.tail.load(Ordering::Acquire);
        let cursor = self.cursor.load(Ordering::Acquire);
        if self.capacity - head.wrapping_sub(tail) < frames as u64
            || latest.pushed.wrapping_sub(cursor) >= self.segments.len() as u64
        {
            self.overflows.fetch_add(1, Ordering::Relaxed);
            self.split.store(true, Ordering::Relaxed);
            return false;
        }
        let expected = self.next_capture.load(Ordering::Relaxed);
        let (continues, next) = match captured {
            Captured::Measured(at) => (
                expected == NO_TIME || at.abs_diff(expected) as f64 <= self.tolerance_ticks,
                at + (frames as f64 * self.ticks_per_frame).round() as u64,
            ),
            Captured::Unknown | Captured::Estimated(_) => (true, NO_TIME),
        };
        self.next_capture.store(next, Ordering::Relaxed);
        let mut published = Published {
            head: head.wrapping_add(frames as u64),
            pushed: latest.pushed.wrapping_add(1),
            ..latest
        };
        if self.split.swap(false, Ordering::Relaxed) || !continues {
            published.run = latest.run.wrapping_add(1);
            published.run_start = head;
            published.run_segment = latest.pushed;
            if self.started.swap(true, Ordering::Relaxed) {
                self.discontinuities.fetch_add(1, Ordering::Relaxed);
            }
        }
        for frame in 0..frames {
            let base = (head.wrapping_add(frame as u64) as usize & self.mask) * CHANNELS;
            for ch in 0..CHANNELS {
                unsafe { *self.samples[base + ch].get() = sample(frame, ch) };
            }
        }
        self.segment(latest.pushed).write(Push {
            start: head,
            len: frames as u64,
            run: published.run,
            captured: match captured {
                Captured::Unknown => NO_TIME,
                Captured::Measured(at) | Captured::Estimated(at) => at,
            },
            arrival: now,
        });
        self.publication.write(published);
        true
    }

    /// Claude: producer only. The next push starts a run, as after audio the
    /// producer lost or a capture it restarted.
    pub fn discontinuity(&self) {
        self.split.store(true, Ordering::Relaxed);
    }

    /// Claude: consumer only. The latest publication, or the one it read last
    /// while the producer is in the middle of publishing.
    fn view(&self) -> Published {
        let seen = unsafe { &mut *self.seen.get() };
        if let Some(published) = (0..READ_ATTEMPTS).find_map(|_| self.publication.read()) {
            *seen = published;
        }
        *seen
    }

    /// Claude: consumer only. Where the run it took ends as far as `published`
    /// shows, in frames and in segments: past its last segment, which it finds
    /// by the runs the segments record, from where it last looked.
    fn run_end(&self, published: &Published) -> (u64, u64) {
        let taken = self.taken.load(Ordering::Relaxed);
        let mut reach = self.reach.load(Ordering::Relaxed);
        while ahead(published.pushed, reach) > 0 && self.segment(reach).read().run == taken {
            reach = reach.wrapping_add(1);
        }
        self.reach.store(reach, Ordering::Relaxed);
        let last = self.segment(reach.wrapping_sub(1)).read();
        (last.start.wrapping_add(last.len), reach)
    }

    /// Claude: consumer only. The tail and how many frames from it the consumer
    /// may read: up to the end of the run it took.
    fn readable(&self, published: &Published) -> (u64, usize) {
        let tail = self.tail.load(Ordering::Relaxed);
        if !self.reading.load(Ordering::Relaxed) {
            return (tail, 0);
        }
        let (end, _) = self.run_end(published);
        (tail, ahead(end, tail).max(0) as usize)
    }

    /// Claude: consumer only. The record of the push the frame at the tail
    /// came in, or of the last one of its run when the consumer has read all
    /// the run holds, moving the cursor up to it.
    fn current(&self, published: &Published) -> Option<Push> {
        if !self.reading.load(Ordering::Relaxed) {
            return None;
        }
        let tail = self.tail.load(Ordering::Relaxed);
        let (_, segments) = self.run_end(published);
        let mut cursor = self.cursor.load(Ordering::Relaxed);
        let mut push = self.segment(cursor).read();
        while ahead(tail, push.start.wrapping_add(push.len)) >= 0
            && ahead(segments, cursor.wrapping_add(1)) > 0
        {
            cursor = cursor.wrapping_add(1);
            push = self.segment(cursor).read();
        }
        self.cursor.store(cursor, Ordering::Release);
        Some(push)
    }

    /// Claude: consumer only. How many frames it may read now.
    pub fn available(&self) -> usize {
        self.readable(&self.view()).1
    }

    /// Claude: consumer only. Drops what the producer pushed before its latest
    /// discontinuity, if there was one since the last call, and returns when
    /// the frame now at the tail, the new run's first, was captured, as
    /// `tail_time` would.
    pub fn take_discontinuity(&self) -> Option<f64> {
        let published = self.view();
        if published.run == self.taken.load(Ordering::Relaxed) {
            return None;
        }
        let tail = self.tail.load(Ordering::Relaxed);
        if !self.reading.load(Ordering::Relaxed) || ahead(published.run_start, tail) > 0 {
            self.tail.store(published.run_start, Ordering::Release);
        }
        self.cursor.store(published.run_segment, Ordering::Release);
        self.reach
            .store(published.run_segment.wrapping_add(1), Ordering::Relaxed);
        self.taken.store(published.run, Ordering::Relaxed);
        self.reading.store(true, Ordering::Relaxed);
        Some(self.time_at_tail(&published))
    }

    /// Claude: consumer only. The frames buffered up to the end of the latest
    /// push of the run it took, and when that push arrived.
    pub fn last_push(&self) -> LastPush {
        let published = self.view();
        if !self.reading.load(Ordering::Relaxed) {
            return LastPush { buffered: 0, at: 0 };
        }
        let (_, segments) = self.run_end(&published);
        let last = self.segment(segments.wrapping_sub(1)).read();
        let tail = self.tail.load(Ordering::Relaxed);
        LastPush {
            buffered: ahead(last.start.wrapping_add(last.len), tail).max(0) as usize,
            at: last.arrival,
        }
    }

    /// Claude: consumer only. When the frame at the tail was captured, as the
    /// arrival of the push it came in tells, as an instant: now when the
    /// consumer took no run yet. Stamping packets with it makes the receiver
    /// release them at the source's cadence, not the encoder's.
    pub fn captured_at(&self, _sample_rate: f64, ticks_per_second: f64) -> Instant {
        let now = Instant::now();
        let Some(push) = self.current(&self.view()) else {
            return now;
        };
        let arrived = self.arrived(&push);
        let age = (host_time::now() as f64 - arrived).max(0.0) / ticks_per_second;
        now.checked_sub(Duration::from_secs_f64(age)).unwrap_or(now)
    }

    /// Claude: consumer only. The host time at which the frame at the tail was
    /// captured, by the capture time of the push it came in and the frames
    /// between, at `ticks_per_frame`; past the last frame of its run, as if the
    /// run went on. None when that push carried no capture time.
    pub fn tail_captured(&self, ticks_per_frame: f64) -> Option<u64> {
        let push = self.current(&self.view())?;
        self.dated(&push, ticks_per_frame).map(|at| at as u64)
    }

    fn dated(&self, push: &Push, ticks_per_frame: f64) -> Option<f64> {
        if push.captured == NO_TIME {
            return None;
        }
        let tail = self.tail.load(Ordering::Relaxed);
        let frames = ahead(tail, push.start);
        Some((push.captured as f64 + frames as f64 * ticks_per_frame).max(0.0))
    }

    /// Claude: when the frame at the tail was captured as the arrival of its
    /// push tells: the push's last frame just before it arrived.
    fn arrived(&self, push: &Push) -> f64 {
        let tail = self.tail.load(Ordering::Relaxed);
        let after = ahead(push.start.wrapping_add(push.len), tail);
        push.arrival as f64 - after as f64 * self.ticks_per_frame
    }

    /// Claude: consumer only. When the frame at the tail was captured, in host
    /// ticks: its date when its push carried one, else as its push's arrival
    /// tells. Host time now before the consumer took a run.
    pub fn tail_time(&self) -> f64 {
        self.time_at_tail(&self.view())
    }

    fn time_at_tail(&self, published: &Published) -> f64 {
        match self.current(published) {
            Some(push) => self
                .dated(&push, self.ticks_per_frame)
                .unwrap_or_else(|| self.arrived(&push)),
            None => host_time::now() as f64,
        }
    }

    /// Claude: consumer only. Fills `dst` with up to `dst.len() / CHANNELS` frames
    /// and returns how many it read.
    pub fn pop_into(&self, dst: &mut [f32]) -> usize {
        let published = self.view();
        let (tail, readable) = self.readable(&published);
        let frames = readable.min(dst.len() / CHANNELS);
        for frame in 0..frames {
            let base = (tail.wrapping_add(frame as u64) as usize & self.mask) * CHANNELS;
            for ch in 0..CHANNELS {
                dst[frame * CHANNELS + ch] = unsafe { *self.samples[base + ch].get() };
            }
        }
        self.tail
            .store(tail.wrapping_add(frames as u64), Ordering::Release);
        self.current(&published);
        frames
    }

    /// Claude: consumer only.
    pub fn discard(&self, frames: usize) -> usize {
        let published = self.view();
        let (tail, readable) = self.readable(&published);
        let frames = readable.min(frames);
        self.tail
            .store(tail.wrapping_add(frames as u64), Ordering::Release);
        self.current(&published);
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48_000.0;

    fn ticks(seconds: f64) -> u64 {
        (seconds * host_time::ticks_per_second()) as u64
    }

    fn push_counting(ring: &Ring, start: usize, frames: usize) -> bool {
        ring.push_with(frames, 0, Captured::Unknown, |f, ch| {
            ((start + f) * CHANNELS + ch) as f32
        })
    }

    /// Claude: pushes `frames` frames of `value` captured at `seconds`, on a
    /// clock far enough along that dates do not saturate at zero.
    fn push_measured(ring: &Ring, seconds: f64, frames: usize, value: f32) -> bool {
        ring.push_with(
            frames,
            0,
            Captured::Measured(ticks(100.0 + seconds)),
            |_, _| value,
        )
    }

    fn tail_seconds(ring: &Ring) -> Option<f64> {
        ring.tail_captured(host_time::ticks_per_second() / RATE)
            .map(|at| at as f64 / host_time::ticks_per_second() - 100.0)
    }

    fn front(ring: &Ring) -> f32 {
        let mut frame = [0.0; CHANNELS];
        assert_eq!(ring.pop_into(&mut frame), 1);
        frame[0]
    }

    #[test]
    fn frames_come_out_in_order_across_the_wrap() {
        let ring = Ring::new(RATE);
        let mut written = 0;
        let mut read = 0;
        let mut dst = vec![0.0; 1000 * CHANNELS];
        while written <= CAPACITY_FRAMES * 3 {
            assert!(push_counting(&ring, written, 700));
            written += 700;
            ring.take_discontinuity();
            let frames = ring.pop_into(&mut dst);
            for (i, sample) in dst[..frames * CHANNELS].iter().enumerate() {
                assert_eq!(*sample, (read * CHANNELS + i) as f32);
            }
            read += frames;
        }
        assert_eq!(written, read + ring.available());
    }

    #[test]
    fn refuses_pushes_that_do_not_fit() {
        let ring = Ring::new(RATE);
        assert!(push_counting(&ring, 0, CAPACITY_FRAMES - 10));
        assert!(!push_counting(&ring, 0, 11));
        assert!(push_counting(&ring, 0, 10));
        assert!(ring.take_discontinuity().is_some());
        assert_eq!(ring.available(), 10);
    }

    #[test]
    fn discard_drops_the_oldest_frames() {
        let ring = Ring::new(RATE);
        push_counting(&ring, 0, 10);
        assert!(ring.take_discontinuity().is_some());
        assert_eq!(ring.discard(4), 4);
        let mut dst = [0.0; CHANNELS];
        ring.pop_into(&mut dst);
        assert_eq!(dst[0], (4 * CHANNELS) as f32);
        assert_eq!(ring.discard(100), 5);
        assert_eq!(ring.available(), 0);
    }

    #[test]
    fn reports_frames_buffered_up_to_the_latest_push() {
        let ring = Ring::new(RATE);
        assert!(ring.push_with(100, 7, Captured::Unknown, |_, _| 0.0));
        assert!(ring.push_with(50, (1u64 << 32) + 9, Captured::Unknown, |_, _| 0.0));
        assert!(ring.take_discontinuity().is_some());
        let mut dst = vec![0.0; 30 * CHANNELS];
        ring.pop_into(&mut dst);
        let last = ring.last_push();
        assert_eq!((last.buffered, last.at), (120, (1u64 << 32) + 9));
    }

    #[test]
    fn dates_each_frame_by_the_push_it_came_in() {
        let ring = Ring::new(RATE);
        assert_eq!(tail_seconds(&ring), None);
        assert!(push_measured(&ring, 0.0, 480, 0.0));
        assert!(push_measured(&ring, 0.01, 480, 0.0));
        assert_eq!(tail_seconds(&ring), None, "dated before it was taken");
        assert!(ring.take_discontinuity().is_some());
        let date = tail_seconds(&ring).unwrap();
        assert!(date.abs() < 1e-6, "{date}");
        ring.discard(720);
        let date = tail_seconds(&ring).unwrap();
        assert!((date - 0.015).abs() < 1e-6, "{date}");
        assert!(ring.push_with(10, 0, Captured::Unknown, |_, _| 0.0));
        let date = tail_seconds(&ring).unwrap();
        assert!((date - 0.015).abs() < 1e-6, "{date}");
        ring.discard(240);
        assert_eq!(
            tail_seconds(&ring),
            None,
            "dated a frame its push did not date"
        );
        assert!(ring.take_discontinuity().is_none());
    }

    #[test]
    fn measured_pushes_within_the_tolerance_continue_one_run() {
        let ring = Ring::new(RATE);
        assert!(push_measured(&ring, 0.0, 480, 1.0));
        assert!(push_measured(&ring, 0.0105, 480, 2.0));
        assert!(ring.take_discontinuity().is_some());
        assert!(ring.take_discontinuity().is_none());
        assert_eq!(ring.available(), 960);
        assert_eq!(ring.discontinuities(), 0);
    }

    /// Claude: the controller's first reproduction: a 10 ms gap between two
    /// pushes must not re-date the first one, still queued, by 10 ms.
    #[test]
    fn a_gap_in_capture_never_redates_what_was_queued_before_it() {
        let ring = Ring::new(RATE);
        assert!(push_measured(&ring, 0.0, 480, 1.0));
        assert!(push_measured(&ring, 0.02, 480, 2.0));
        assert_eq!(tail_seconds(&ring), None, "the first block went undated");
        assert!(ring.take_discontinuity().is_some());
        assert_eq!(front(&ring), 2.0, "the first block was dropped");
        let date = tail_seconds(&ring).unwrap();
        assert!((date - (0.02 + 1.0 / RATE)).abs() < 1e-6, "{date}");
        assert_eq!(ring.discontinuities(), 1);
    }

    /// Claude: the controller's second reproduction: after one push is refused
    /// for lack of room, accepting the next must not re-date the backlog.
    #[test]
    fn an_overflowed_push_ends_the_run_before_it() {
        let ring = Ring::new(RATE);
        let block = 480;
        let blocks = CAPACITY_FRAMES / block;
        for i in 0..blocks {
            assert!(push_measured(&ring, i as f64 * 0.01, block, 1.0));
            if i == 0 {
                assert!(ring.take_discontinuity().is_some());
            }
        }
        let mut next = blocks as f64 * 0.01;
        while push_measured(&ring, next, block, 1.0) {
            next += 0.01;
        }
        assert_eq!(ring.overflows(), 1);
        ring.discard(block);
        assert!(push_measured(&ring, next + 0.01, block, 2.0));
        let backlog = tail_seconds(&ring).unwrap();
        assert!(
            (backlog - 0.01).abs() < 1e-6,
            "the backlog dated {backlog} s"
        );
        assert!(ring.take_discontinuity().is_some());
        assert_eq!(ring.available(), block);
        assert_eq!(front(&ring), 2.0);
        let date = tail_seconds(&ring).unwrap();
        assert!((date - (next + 0.01 + 1.0 / RATE)).abs() < 1e-6, "{date}");
    }

    #[test]
    fn an_overflow_ends_the_run_of_a_source_that_does_not_measure_its_dates() {
        for captured in [Captured::Unknown, Captured::Estimated(ticks(100.0))] {
            let ring = Ring::new(RATE);
            assert!(ring.push_with(480, 0, captured, |_, _| 1.0));
            assert!(ring.take_discontinuity().is_some());
            while ring.push_with(480, 0, captured, |_, _| 1.0) {}
            ring.discard(480);
            assert!(ring.push_with(480, 0, captured, |_, _| 2.0));
            assert!(ring.take_discontinuity().is_some(), "{captured:?}");
            assert_eq!(ring.available(), 480);
            assert_eq!(front(&ring), 2.0);
        }
    }

    #[test]
    fn a_marked_discontinuity_ends_the_run_however_the_pushes_are_dated() {
        let ring = Ring::new(RATE);
        assert!(push_measured(&ring, 0.0, 480, 1.0));
        ring.discontinuity();
        assert!(push_measured(&ring, 0.01, 480, 2.0));
        assert!(ring.take_discontinuity().is_some());
        assert_eq!(front(&ring), 2.0);
    }

    #[test]
    fn estimated_dates_may_move_without_ending_the_run_each_push_keeping_its_own() {
        let ring = Ring::new(RATE);
        let estimated = |seconds| Captured::Estimated(ticks(100.0 + seconds));
        assert!(ring.push_with(480, 0, estimated(0.0), |_, _| 1.0));
        assert!(ring.push_with(480, 0, estimated(0.015), |_, _| 2.0));
        assert!(ring.take_discontinuity().is_some());
        assert!(ring.take_discontinuity().is_none());
        let date = tail_seconds(&ring).unwrap();
        assert!(date.abs() < 1e-6, "{date}");
        assert_eq!(front(&ring), 1.0);
        ring.discard(479);
        let date = tail_seconds(&ring).unwrap();
        assert!((date - 0.015).abs() < 1e-6, "{date}");
    }

    #[test]
    fn the_consumer_never_reads_into_a_run_it_has_not_taken() {
        let ring = Ring::new(RATE);
        assert!(push_measured(&ring, 0.0, 480, 1.0));
        assert_eq!(ring.available(), 0, "read before it was taken");
        assert!(ring.take_discontinuity().is_some());
        assert!(push_measured(&ring, 0.02, 480, 2.0));
        assert!(push_measured(&ring, 0.04, 480, 3.0));
        assert_eq!(ring.available(), 480);
        let mut block = vec![0.0; 960 * CHANNELS];
        assert_eq!(ring.pop_into(&mut block), 480);
        assert!(block[..480 * CHANNELS].iter().all(|s| *s == 1.0));
        assert_eq!(ring.available(), 0);
        assert!(ring.take_discontinuity().is_some());
        assert_eq!(ring.available(), 480);
        assert_eq!(front(&ring), 3.0);
        assert!(ring.take_discontinuity().is_none());
    }

    /// Claude: the controller's 512 resets: one-frame pushes, each after a
    /// marked discontinuity, while the consumer is stopped. Taking the
    /// discontinuity leaves only the latest run, dated by its own capture.
    #[test]
    fn a_stopped_consumer_takes_only_the_latest_of_many_runs() {
        for runs in [511, 512, 513, 4096] {
            let ring = Ring::new(RATE);
            assert!(push_measured(&ring, 0.0, 1, 0.0));
            for run in 1..=runs {
                ring.discontinuity();
                assert!(push_measured(&ring, run as f64 * 0.002, 1, run as f32));
            }
            assert!(ring.take_discontinuity().is_some(), "{runs} runs");
            assert_eq!(ring.available(), 1, "{runs} runs");
            let date = tail_seconds(&ring).unwrap();
            assert!(
                (date - runs as f64 * 0.002).abs() < 1e-6,
                "{runs} runs: {date}"
            );
            assert_eq!(front(&ring), runs as f32);
            assert!(ring.take_discontinuity().is_none());
        }
    }

    /// Claude: a capture date keeps all its bits, so audio captured half or a
    /// whole turn of a 32-bit tick count before it arrived stays dated then.
    #[test]
    fn a_capture_date_keeps_its_epoch() {
        let arrival = ticks(10_000.0);
        for ago in [0, 1 << 31, 1 << 32, 3 << 31] {
            let ring = Ring::new(RATE);
            let captured = arrival - ago;
            assert!(ring.push_with(480, arrival, Captured::Measured(captured), |_, _| 0.0));
            assert!(ring.take_discontinuity().is_some());
            let dated = ring.tail_captured(host_time::ticks_per_second() / RATE);
            assert_eq!(
                dated,
                Some(captured),
                "captured {ago} ticks before it arrived"
            );
        }
    }

    /// Claude: positions and runs near the end of their counts carry on across
    /// it: frames come out in order, dated, and runs end where they should.
    #[test]
    fn positions_and_runs_carry_on_across_the_end_of_their_counts() {
        let ring = Ring::starting_at(RATE, u64::MAX - 1000, u64::MAX - 1, CAPACITY_FRAMES);
        for block in 0..4 {
            assert!(push_measured(&ring, block as f64 * 0.01, 480, block as f32));
        }
        assert!(ring.take_discontinuity().is_some());
        assert_eq!(ring.available(), 1920);
        assert_eq!(front(&ring), 0.0);
        ring.discard(479);
        let date = tail_seconds(&ring).unwrap();
        assert!((date - 0.01).abs() < 1e-6, "{date}");
        for run in 1..4 {
            ring.discontinuity();
            assert!(push_measured(&ring, 1.0 + run as f64, 1, 10.0 + run as f32));
        }
        assert!(ring.take_discontinuity().is_some());
        assert_eq!(ring.available(), 1);
        assert_eq!(front(&ring), 13.0);
        assert!(ring.take_discontinuity().is_none());
    }

    /// Claude: a consumer still reading the run it took keeps dating it after
    /// the producer starts the next, and never dates it by the next one's.
    #[test]
    fn the_run_being_read_stays_dated_after_the_next_one_starts() {
        let ring = Ring::new(RATE);
        assert!(push_measured(&ring, 0.0, 960, 1.0));
        assert!(ring.take_discontinuity().is_some());
        assert!(tail_seconds(&ring).unwrap().abs() < 1e-6);
        assert!(push_measured(&ring, 1.0, 480, 2.0));
        ring.discard(480);
        let date = tail_seconds(&ring).unwrap();
        assert!((date - 0.01).abs() < 1e-6, "{date}");
        assert_eq!(front(&ring), 1.0);
        let fresh = Ring::new(RATE);
        assert!(push_measured(&fresh, 0.0, 960, 1.0));
        assert!(push_measured(&fresh, 1.0, 480, 2.0));
        assert_eq!(tail_seconds(&fresh), None, "dated by a run it never saw");
    }

    /// Claude: a run's frames are dated by the latest push the consumer saw of
    /// that run: measured dates that run a little fast by their own date, and
    /// undated frames by when their own push arrived.
    #[test]
    fn a_run_running_fast_or_arriving_late_is_dated_push_by_push() {
        let ring = Ring::new(RATE);
        assert!(push_measured(&ring, 0.0, 480, 0.0));
        assert!(ring.take_discontinuity().is_some());
        for push in 1..10 {
            assert!(push_measured(&ring, push as f64 * 0.0105, 480, 0.0));
        }
        ring.discard(9 * 480);
        let date = tail_seconds(&ring).unwrap();
        assert!((date - 0.0945).abs() < 1e-6, "{date}");

        let ring = Ring::new(RATE);
        assert!(ring.push_with(480, ticks(100.01), Captured::Unknown, |_, _| 0.0));
        assert!(ring.take_discontinuity().is_some());
        assert!(ring.push_with(480, ticks(100.5), Captured::Unknown, |_, _| 0.0));
        ring.discard(480);
        let date = ring.tail_time() / host_time::ticks_per_second();
        assert!((date - 100.49).abs() < 1e-6, "{date}");
    }

    /// Claude: a ring keeps a record per push it holds, one per 16 frames of
    /// its capacity. A push with no free record is refused like one with no
    /// room, and the records already held keep dating their frames.
    #[test]
    fn a_push_without_a_free_record_is_refused_and_ends_the_run() {
        let ring = Ring::with_capacity(RATE, 1024);
        assert!(push_measured(&ring, 0.0, 1, 0.0));
        assert!(ring.take_discontinuity().is_some());
        for push in 1..64 {
            assert!(
                push_measured(&ring, push as f64 / RATE, 1, push as f32),
                "push {push}"
            );
        }
        assert!(!push_measured(&ring, 64.0 / RATE, 1, 64.0));
        assert_eq!(ring.overflows(), 1);
        for frame in 0..64 {
            let date = tail_seconds(&ring).unwrap();
            assert!(
                (date - frame as f64 / RATE).abs() < 1e-6,
                "frame {frame}: {date}"
            );
            assert_eq!(front(&ring), frame as f32);
        }
        assert!(push_measured(&ring, 65.0 / RATE, 1, 65.0));
        assert!(ring.take_discontinuity().is_some());
        assert_eq!(front(&ring), 65.0);
    }

    /// Claude: records the consumer has read are free for the producer again.
    #[test]
    fn records_are_reused_once_read() {
        let ring = Ring::with_capacity(RATE, 1024);
        assert!(push_measured(&ring, 0.0, 4, 0.0));
        assert!(ring.take_discontinuity().is_some());
        for push in 1..1000 {
            assert!(
                push_measured(&ring, push as f64 * 4.0 / RATE, 4, push as f32),
                "push {push}"
            );
            assert_eq!(ring.discard(4), 4);
            let date = tail_seconds(&ring).unwrap();
            assert!(
                (date - push as f64 * 4.0 / RATE).abs() < 1e-6,
                "push {push}: {date}"
            );
        }
        assert_eq!(ring.overflows(), 0);
    }

    #[test]
    fn counts_overflows() {
        let ring = Ring::new(RATE);
        assert!(push_counting(&ring, 0, CAPACITY_FRAMES));
        assert!(!push_counting(&ring, 0, 1));
        assert_eq!(ring.overflows(), 1);
    }
}
