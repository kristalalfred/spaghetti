//! Claude: a calibration leg's source: dated silence, and the probe trains
//! scheduled into it, pushed into the leg's ring as a live capture would be,
//! each block once all of it was captured.

use std::sync::Arc;

use crate::delivery::DeliverySpan;
use crate::host_time;
use crate::ring::{Captured, Ring};
use crate::sinks::SourceTiming;

use super::Role;
use super::probe::{PROBES, layout, probe};

pub const RATE: f64 = 48_000.0;
const BLOCK: u64 = 480;

/// Claude: how the emitter feeds a route: at `RATE`, each frame landing at
/// most two blocks after its capture.
pub fn timing() -> SourceTiming {
    SourceTiming {
        sample_rate: RATE,
        delivery: Arc::new(DeliverySpan::new(2.0 * BLOCK as f64)),
    }
}

pub struct Emitter {
    ring: Arc<Ring>,
    /// Claude: when frame 0 was captured.
    origin: u64,
    ticks_per_frame: f64,
    /// Claude: the next frame to push.
    next: u64,
    probe: Vec<f32>,
    layout: [f64; PROBES],
    /// Claude: where each scheduled probe starts.
    starts: Vec<u64>,
}

impl Emitter {
    /// Claude: an emitter playing `role`'s probe on `role`'s layout.
    pub fn new(ring: Arc<Ring>, origin: u64, role: Role) -> Self {
        Self {
            ring,
            origin,
            ticks_per_frame: host_time::ticks_per_second() / RATE,
            next: 0,
            probe: probe(RATE, role),
            layout: layout(role),
            starts: Vec::new(),
        }
    }

    /// Claude: when `frame` was captured.
    pub fn date(&self, frame: u64) -> u64 {
        self.origin + (frame as f64 * self.ticks_per_frame).round() as u64
    }

    /// Claude: pushes every whole block captured by `now`.
    pub fn feed(&mut self, now: u64) {
        while self.date(self.next + BLOCK) <= now {
            let first = self.next;
            self.ring.push_with(
                BLOCK as usize,
                now,
                Captured::Measured(self.date(first)),
                |frame, _| self.sample(first + frame as u64),
            );
            self.next += BLOCK;
        }
    }

    /// Claude: schedules a train to start at the first frame captured at or
    /// after `at`, and returns when that frame was captured: None when the
    /// emitter pushed that frame already.
    pub fn schedule(&mut self, at: u64) -> Option<u64> {
        let after = at.saturating_sub(self.origin) as f64 / self.ticks_per_frame;
        let first = after.ceil() as u64;
        if first < self.next {
            return None;
        }
        self.starts.extend(
            self.layout
                .iter()
                .map(|offset| first + (offset * RATE).round() as u64),
        );
        Some(self.date(first))
    }

    fn sample(&self, frame: u64) -> f32 {
        self.starts
            .iter()
            .filter_map(|&start| {
                let into = frame.checked_sub(start)?;
                self.probe.get(into as usize)
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::CHANNELS;

    fn seconds(ticks: f64) -> f64 {
        ticks / host_time::ticks_per_second()
    }

    #[test]
    fn frames_are_pushed_dated_once_captured_in_one_run() {
        let ring = Arc::new(Ring::new(RATE));
        let origin = host_time::now();
        let mut emitter = Emitter::new(Arc::clone(&ring), origin, Role::Reference);
        emitter.feed(origin + (0.0099 * host_time::ticks_per_second()) as u64);
        assert!(
            ring.take_discontinuity().is_none(),
            "pushed before captured"
        );
        emitter.feed(emitter.date(10 * BLOCK));
        assert!(ring.take_discontinuity().is_some());
        let mut dst = vec![0.0; 1000 * CHANNELS];
        let mut frame = 0;
        while let Some(read) = ring.pop_push(&mut dst) {
            let date = read.push.captured.unwrap() as f64;
            let expected = emitter.date(frame - read.offset as u64) as f64;
            assert!(seconds((date - expected).abs()) < 1e-9);
            assert!(dst[..read.frames * CHANNELS].iter().all(|s| *s == 0.0));
            frame += read.frames as u64;
        }
        assert_eq!(frame, 10 * BLOCK);
        assert_eq!(ring.discontinuities(), 0);
    }

    #[test]
    fn a_train_plays_its_roles_probes_on_its_roles_layout_from_the_frame_scheduled() {
        for role in [Role::Reference, Role::Output] {
            plays_from_the_frame_scheduled(role);
        }
    }

    fn plays_from_the_frame_scheduled(role: Role) {
        let ring = Arc::new(Ring::new(RATE));
        let origin = 1_000_000;
        let mut emitter = Emitter::new(Arc::clone(&ring), origin, role);
        emitter.feed(emitter.date(BLOCK));
        assert_eq!(emitter.schedule(emitter.date(BLOCK - 1)), None);
        let start = 1000;
        let at = emitter.date(start) - 1;
        assert_eq!(emitter.schedule(at), Some(emitter.date(start)));
        let end = (start + (4.0 * RATE) as u64).next_multiple_of(BLOCK);
        emitter.feed(emitter.date(end));
        assert!(ring.take_discontinuity().is_some());
        let mut played = vec![0.0; end as usize * CHANNELS];
        let mut read = 0;
        while read < end as usize {
            read += ring.pop_into(&mut played[read * CHANNELS..]);
        }
        let probe = probe(RATE, role);
        let left: Vec<f32> = played.iter().step_by(CHANNELS).copied().collect();
        for offset in layout(role) {
            let first = (start + (offset * RATE).round() as u64) as usize;
            assert_eq!(left[first..first + probe.len()], probe[..]);
            assert_eq!(left[first - 1], 0.0);
            assert_eq!(left[first + probe.len()], 0.0);
        }
        let probe_frames = PROBES * probe.len();
        let sounding = left.iter().filter(|s| **s != 0.0).count();
        assert!(sounding <= probe_frames && sounding > probe_frames - 20);
    }
}
