//! Claude: the calibration's recording: everything its ring delivers, kept
//! with the record of each push it came in, and the checks a stretch of it
//! must pass before anything is measured in it.

use std::sync::Arc;

use crate::host_time;
use crate::ring::{CHANNELS, Ring};

use super::RecordingFailure;

/// Claude: how far a program input's push may be dated off the line through
/// its neighbours, in frames, and how far that line's rate may run off the
/// input's own.
const DATE_TOLERANCE_FRAMES: f64 = 0.25;
const RATE_TOLERANCE: f64 = 1e-3;
/// Claude: this many consecutive samples at this level or above are clipped.
const CLIP_LEVEL: f32 = 0.99;
const CLIP_RUN: usize = 3;
const SCRATCH_FRAMES: usize = 4096;

/// Claude: one push as the track keeps it, `start` counting frames from the
/// recording's first.
#[derive(Debug, Clone, Copy)]
struct Pushed {
    start: u64,
    frames: usize,
    run: u64,
    captured: Option<u64>,
    position: Option<u64>,
}

impl Pushed {
    fn end(&self) -> u64 {
        self.start + self.frames as u64
    }
}

pub struct Recorder {
    ring: Arc<Ring>,
    scratch: Vec<f32>,
    track: Track,
}

impl Recorder {
    pub fn new(ring: Arc<Ring>, sample_rate: f64) -> Self {
        Self {
            ring,
            scratch: vec![0.0; SCRATCH_FRAMES * CHANNELS],
            track: Track {
                ticks_per_frame: host_time::ticks_per_second() / sample_rate,
                first: 0,
                left: Vec::new(),
                right: Vec::new(),
                pushes: Vec::new(),
            },
        }
    }

    /// Claude: moves all the ring holds into the track and returns how many
    /// frames that was. It reads each run to its end before taking the next.
    pub fn drain(&mut self) -> usize {
        let mut moved = 0;
        loop {
            let Some(read) = self.ring.pop_push(&mut self.scratch) else {
                if self.ring.take_discontinuity().is_some() {
                    continue;
                }
                return moved;
            };
            let track = &mut self.track;
            if read.offset == 0 || track.pushes.is_empty() {
                let skipped = read.offset as u64;
                track.pushes.push(Pushed {
                    start: track.first + track.left.len() as u64,
                    frames: read.push.frames - read.offset,
                    run: read.push.run,
                    captured: read
                        .push
                        .captured
                        .map(|at| at + (skipped as f64 * track.ticks_per_frame).round() as u64),
                    position: read.push.position.map(|at| at + skipped),
                });
            }
            for frame in self.scratch[..read.frames * CHANNELS].chunks(CHANNELS) {
                track.left.push(frame[0]);
                track.right.push(frame[1]);
            }
            moved += read.frames;
        }
    }

    pub fn track(&self) -> &Track {
        &self.track
    }
}

pub struct Track {
    ticks_per_frame: f64,
    /// Claude: the frame `left[0]` and `right[0]` hold.
    first: u64,
    left: Vec<f32>,
    right: Vec<f32>,
    pushes: Vec<Pushed>,
}

/// Claude: a checked stretch of the recording, mixed to mono.
pub struct Span {
    pub samples: Vec<f32>,
    /// Claude: when each sample was captured, by the push it came in.
    dates: Vec<(u64, f64)>,
    ticks_per_frame: f64,
}

impl Span {
    /// Claude: when the sample at fractional `index` was captured, in host ticks.
    pub fn date(&self, index: f64) -> f64 {
        let at = self
            .dates
            .partition_point(|(start, _)| *start as f64 <= index)
            .saturating_sub(1);
        let (start, date) = self.dates[at];
        date + (index - start as f64) * self.ticks_per_frame
    }
}

impl Track {
    /// Claude: when the frame after the latest was captured, as far as the
    /// latest push dates it.
    pub fn end_date(&self) -> Option<u64> {
        let last = self.pushes.last()?;
        let end = self.first + self.left.len() as u64;
        last.captured
            .map(|at| at + ((end - last.start) as f64 * self.ticks_per_frame) as u64)
    }

    /// Claude: the recording from `from` to `to`, once it is all there and
    /// passes every check: each push dated, one run, every frame captured once
    /// (by the device's count, or, for a source that does not count, by dates
    /// within a quarter frame of the line through them), nothing clipped or
    /// not finite. None while the recording has not reached `to` yet.
    pub fn span(&self, from: u64, to: u64) -> Option<Result<Span, RecordingFailure>> {
        if self.end_date()? <= to {
            return None;
        }
        Some(self.checked(from, to))
    }

    fn checked(&self, from: u64, to: u64) -> Result<Span, RecordingFailure> {
        let covering = |at: u64| {
            self.pushes
                .iter()
                .rposition(|push| push.captured.is_some_and(|captured| captured <= at))
        };
        let (Some(first), Some(last)) = (covering(from), covering(to)) else {
            return Err(RecordingFailure::Gap);
        };
        let pushes = &self.pushes[first..=last];
        if pushes.iter().any(|push| push.captured.is_none()) {
            return Err(RecordingFailure::Undated);
        }
        let contiguous = pushes.windows(2).all(|pair| {
            pair[0].run == pair[1].run
                && pair[0].end() == pair[1].start
                && match (pair[0].position, pair[1].position) {
                    (Some(a), Some(b)) => a + pair[0].frames as u64 == b,
                    _ => true,
                }
        });
        if !contiguous {
            return Err(RecordingFailure::Gap);
        }
        if pushes.iter().any(|push| push.position.is_none()) && !self.on_one_line(pushes) {
            return Err(RecordingFailure::Gap);
        }
        let index = |push: &Pushed, at: u64| {
            let captured = push.captured.expect("checked dated") as f64;
            push.start + ((at as f64 - captured) / self.ticks_per_frame) as u64
        };
        let begin = (index(&pushes[0], from) - self.first) as usize;
        let end = (index(&pushes[pushes.len() - 1], to) - self.first) as usize + 1;
        for channel in [&self.left[begin..end], &self.right[begin..end]] {
            if channel.iter().any(|sample| !sample.is_finite()) {
                return Err(RecordingFailure::NonFinite);
            }
            let clipped = channel
                .windows(CLIP_RUN)
                .any(|run| run.iter().all(|sample| sample.abs() >= CLIP_LEVEL));
            if clipped {
                return Err(RecordingFailure::Clipped);
            }
        }
        let origin = self.first + begin as u64;
        Ok(Span {
            samples: (begin..end)
                .map(|i| 0.5 * (self.left[i] + self.right[i]))
                .collect(),
            dates: pushes
                .iter()
                .map(|push| {
                    let start = push.start.saturating_sub(origin);
                    let skipped = origin.saturating_sub(push.start) as f64;
                    let captured = push.captured.expect("checked dated") as f64;
                    (start, captured + skipped * self.ticks_per_frame)
                })
                .collect(),
            ticks_per_frame: self.ticks_per_frame,
        })
    }

    /// Claude: whether the pushes' dates lie within `DATE_TOLERANCE_FRAMES` of
    /// the least-squares line through them, at a rate within `RATE_TOLERANCE`
    /// of the recording's own.
    fn on_one_line(&self, pushes: &[Pushed]) -> bool {
        if pushes.len() < 2 {
            return true;
        }
        let (x0, y0) = (pushes[0].start, pushes[0].captured.expect("checked dated"));
        let points: Vec<(f64, f64)> = pushes
            .iter()
            .map(|push| {
                let captured = push.captured.expect("checked dated");
                ((push.start - x0) as f64, captured as f64 - y0 as f64)
            })
            .collect();
        let n = points.len() as f64;
        let (mean_x, mean_y) = points
            .iter()
            .fold((0.0, 0.0), |(x, y), (px, py)| (x + px / n, y + py / n));
        let (sxy, sxx) = points.iter().fold((0.0, 0.0), |(sxy, sxx), (x, y)| {
            (
                sxy + (x - mean_x) * (y - mean_y),
                sxx + (x - mean_x) * (x - mean_x),
            )
        });
        let slope = sxy / sxx;
        let intercept = mean_y - slope * mean_x;
        (slope / self.ticks_per_frame - 1.0).abs() <= RATE_TOLERANCE
            && points.iter().all(|(x, y)| {
                (y - (intercept + slope * x)).abs() <= DATE_TOLERANCE_FRAMES * self.ticks_per_frame
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::Captured;

    const RATE: f64 = 48_000.0;

    /// Claude: a recording fed by `pushes`: each its first frame, its length,
    /// its date in frames off where the first frame dates it, and the level
    /// of its left and right samples.
    struct Fed {
        ring: Arc<Ring>,
        recorder: Recorder,
        origin: u64,
    }

    impl Fed {
        fn new() -> Self {
            let ring = Arc::new(Ring::new(RATE));
            Self {
                recorder: Recorder::new(Arc::clone(&ring), RATE),
                ring,
                origin: 1_000_000_000,
            }
        }

        fn at(&self, frames: f64) -> u64 {
            self.origin + (frames * host_time::ticks_per_second() / RATE).round() as u64
        }

        fn push(&self, frames: usize, captured: Captured, level: [f32; 2]) {
            assert!(self.ring.push_with(frames, 0, captured, |_, ch| level[ch]));
        }

        /// Claude: `count` pushes of `frames`, the first starting at frame
        /// `first`, dated at `ppm` off the nominal rate.
        fn program(&self, first: usize, count: usize, frames: usize, ppm: f64) {
            for push in 0..count {
                let start = (first + push * frames) as f64;
                let date = self.at(start * (1.0 + ppm * 1e-6));
                self.push(frames, Captured::Measured(date), [0.1, 0.1]);
            }
        }

        fn span(&mut self, from: f64, to: f64) -> Option<Result<Span, RecordingFailure>> {
            self.recorder.drain();
            let (from, to) = (self.at(from), self.at(to));
            self.recorder.track().span(from, to)
        }
    }

    #[test]
    fn a_steady_program_recording_passes_and_dates_each_sample() {
        for ppm in [0.0, 500.0, -500.0] {
            let mut fed = Fed::new();
            fed.program(0, 100, 480, ppm);
            let span = fed.span(1000.0, 47_000.0).unwrap().unwrap();
            let frame = host_time::ticks_per_second() / RATE;
            let from = fed.at(1000.0) as f64;
            assert!((0.0..frame).contains(&(from - span.date(0.0))), "{ppm} ppm");
            assert!((span.samples[0] - 0.1).abs() < 1e-6);
        }
    }

    #[test]
    fn a_span_waits_until_the_recording_passes_its_end() {
        let mut fed = Fed::new();
        fed.program(0, 10, 480, 0.0);
        assert!(fed.span(0.0, 4800.0).is_none());
        assert!(fed.span(0.0, 4799.0).is_some());
    }

    #[test]
    fn a_program_recording_running_off_its_rate_fails() {
        let mut fed = Fed::new();
        fed.program(0, 100, 480, 1500.0);
        assert!(matches!(
            fed.span(0.0, 40_000.0),
            Some(Err(RecordingFailure::Gap))
        ));
    }

    #[test]
    fn one_frame_lost_or_repeated_between_program_pushes_fails() {
        for (gap, frames) in [(1.0, 480), (-1.0, 480), (32.0, 32), (-32.0, 32)] {
            let mut fed = Fed::new();
            fed.program(0, 50, frames, 0.0);
            let resumed = 50 * frames;
            for push in 0..50 {
                let start = (resumed + push * frames) as f64 + gap;
                fed.push(frames, Captured::Measured(fed.at(start)), [0.1, 0.1]);
            }
            let end = (100 * frames) as f64 - gap.abs() - 10.0;
            assert!(
                matches!(fed.span(5.0, end), Some(Err(RecordingFailure::Gap))),
                "{gap} frames off between {frames}-frame pushes"
            );
            assert_eq!(fed.ring.discontinuities(), 0);
        }
    }

    #[test]
    fn a_frame_the_device_did_not_count_fails() {
        let mut fed = Fed::new();
        for push in 0..20u64 {
            let start = push * 480 + u64::from(push >= 10);
            let captured = Captured::Counted {
                at: fed.at(start as f64),
                position: 9000 + start,
            };
            fed.push(480, captured, [0.1, 0.1]);
            fed.recorder.drain();
        }
        assert!(matches!(
            fed.span(10.0, 9000.0),
            Some(Err(RecordingFailure::Gap))
        ));
        assert!(fed.span(10.0, 4000.0).unwrap().is_ok());
    }

    #[test]
    fn an_undated_push_fails() {
        let mut fed = Fed::new();
        fed.program(0, 5, 480, 0.0);
        fed.push(480, Captured::Unknown, [0.1, 0.1]);
        fed.program(2880, 5, 480, 0.0);
        assert!(matches!(
            fed.span(10.0, 5000.0),
            Some(Err(RecordingFailure::Undated))
        ));
    }

    #[test]
    fn three_clipped_samples_in_a_row_on_either_channel_fail() {
        for (clipped, fails) in [(2, false), (3, true)] {
            let mut fed = Fed::new();
            fed.program(0, 5, 480, 0.0);
            fed.push(clipped, Captured::Measured(fed.at(2400.0)), [0.2, -0.995]);
            fed.program(2400 + clipped, 5, 480, 0.0);
            let checked = fed.span(10.0, 4000.0).unwrap();
            assert_eq!(
                matches!(checked, Err(RecordingFailure::Clipped)),
                fails,
                "{clipped} samples"
            );
        }
    }

    #[test]
    fn a_sample_that_is_not_finite_fails() {
        let mut fed = Fed::new();
        fed.program(0, 5, 480, 0.0);
        fed.push(1, Captured::Measured(fed.at(2400.0)), [f32::NAN, 0.0]);
        fed.program(2401, 5, 480, 0.0);
        assert!(matches!(
            fed.span(10.0, 4000.0),
            Some(Err(RecordingFailure::NonFinite))
        ));
    }
}
