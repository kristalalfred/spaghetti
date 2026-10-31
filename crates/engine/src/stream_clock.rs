use crate::host_time;

/// Claude: where an encoder's stream stands on its source's capture clock. It
/// follows the ring's tail from pop to pop, so that across a discontinuity in
/// the ring the stream's media time skips what never came instead of splicing
/// the next run onto the last, and a receiver conceals the gap.
pub struct StreamClock {
    ticks_per_frame: f64,
    /// Claude: when the next frame the stream encodes was captured, had its
    /// source run on, in host ticks.
    next: Option<f64>,
    /// Claude: source frames of silence that flush every earlier frame out of
    /// the encoder before its media time may jump.
    flush: usize,
    /// Claude: source frames in one of the encoder's packets.
    packet: f64,
}

/// Claude: how a stream reaches the first frame of a new run.
#[derive(Debug, PartialEq)]
pub enum Crossing {
    /// Claude: the run starts where the stream stands, or the stream has no
    /// place yet.
    Continuous,
    /// Claude: encode `silence` frames of silence, then skip `packets` whole
    /// packets of media time.
    Gap { silence: usize, packets: usize },
    /// Claude: the run repeats this many frames the stream already holds; drop
    /// them from it.
    Overlap(usize),
}

impl StreamClock {
    /// Claude: for an encoder that holds `delay` frames and packs `packet`
    /// frames per packet, both at its own `encoder_rate`, fed at `source_rate`.
    pub fn new(source_rate: f64, encoder_rate: f64, packet: usize, delay: usize) -> Self {
        let to_source = source_rate / encoder_rate;
        Self {
            ticks_per_frame: host_time::ticks_per_second() / source_rate,
            next: None,
            flush: ((2 * packet + delay) as f64 * to_source).ceil() as usize,
            packet: packet as f64 * to_source,
        }
    }

    /// Claude: before popping `frames` frames from a ring's tail, which was
    /// captured at host time `tail`, as `Ring::tail_time` returns it.
    pub fn popping(&mut self, tail: f64, frames: usize) {
        self.next = Some(tail + frames as f64 * self.ticks_per_frame);
    }

    /// Claude: how the stream reaches a new run whose first frame was
    /// captured at host time `start`, as `Ring::take_discontinuity` returns it.
    pub fn cross(&mut self, start: f64) -> Crossing {
        let Some(next) = self.next else {
            return Crossing::Continuous;
        };
        let gap = ((start - next) / self.ticks_per_frame).round();
        if gap < 0.0 {
            return Crossing::Overlap(-gap as usize);
        }
        self.next = Some(start);
        let gap = gap as usize;
        if gap <= self.flush {
            return Crossing::Gap {
                silence: gap,
                packets: 0,
            };
        }
        let packets = ((gap - self.flush) as f64 / self.packet).floor();
        Crossing::Gap {
            silence: gap - (packets * self.packet).round() as usize,
            packets: packets as usize,
        }
    }

    /// Claude: source frames in `packets` packets.
    pub fn packet_frames(&self, packets: usize) -> u64 {
        (packets as f64 * self.packet).round() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::Captured;
    use crate::ring::Ring;

    const RATE: f64 = 48_000.0;

    fn push(ring: &Ring, seconds: f64, frames: usize) {
        let at = ((100.0 + seconds) * host_time::ticks_per_second()) as u64;
        assert!(ring.push_with(frames, 0, Captured::Measured(at), |_, _| 0.0));
    }

    fn crossing_after(gap_seconds: f64) -> Crossing {
        let ring = Ring::new(RATE);
        let mut clock = StreamClock::new(RATE, RATE, 960, 312);
        push(&ring, 0.0, 960);
        assert_eq!(
            clock.cross(ring.take_discontinuity().unwrap()),
            Crossing::Continuous
        );
        clock.popping(ring.tail_time(), 960);
        ring.discard(960);
        push(&ring, 0.02 + gap_seconds, 960);
        clock.cross(ring.take_discontinuity().expect("a discontinuity"))
    }

    /// Claude: the controller's interleaving: the consumer looks for a
    /// discontinuity while the producer is inside the push that starts the new
    /// run, before that push is complete. The crossing must still count the
    /// whole one-second gap.
    #[test]
    fn a_crossing_counts_the_whole_gap_however_the_push_interleaves() {
        use std::sync::{Arc, Barrier};

        let ticks = |seconds: f64| (seconds * host_time::ticks_per_second()) as u64;
        let ring = Arc::new(Ring::new(RATE));
        let mut clock = StreamClock::new(RATE, RATE, 960, 312);
        assert!(ring.push_with(
            960,
            ticks(100.02),
            Captured::Measured(ticks(100.0)),
            |_, _| 0.0
        ));
        assert!(ring.take_discontinuity().is_some());
        clock.popping(ring.tail_time(), 960);
        assert_eq!(ring.pop_into(&mut [0.0; 960 * 2]), 960);

        let (inside, release) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
        let producer = std::thread::spawn({
            let (ring, inside, release) =
                (Arc::clone(&ring), Arc::clone(&inside), Arc::clone(&release));
            move || {
                let mut first = true;
                ring.push_with(
                    960,
                    ticks(101.04),
                    Captured::Measured(ticks(101.02)),
                    |_, _| {
                        if first {
                            first = false;
                            inside.wait();
                            release.wait();
                        }
                        0.0
                    },
                )
            }
        });
        inside.wait();
        let during = ring.take_discontinuity().map(|start| clock.cross(start));
        release.wait();
        assert!(producer.join().unwrap());
        let after = ring.take_discontinuity().map(|start| clock.cross(start));
        let crossings: Vec<Crossing> = during.into_iter().chain(after).collect();
        let [Crossing::Gap { silence, packets }] = crossings[..] else {
            panic!("{crossings:?}");
        };
        assert_eq!(silence + packets * 960, 48_000, "{crossings:?}");
    }

    /// Claude: when the producer publishes the next run between the consumer's
    /// steps for one packet, the packet keeps the date of the run it comes
    /// from, and the crossing counts the whole one-second gap. `step` is how
    /// many of take, availability, date, clock and pop come before the new run;
    /// at 0 the consumer takes the new run first and drops the old.
    fn first_packet_with_the_next_run_published_after(
        step: usize,
        captured: impl Fn(f64) -> Captured,
        mark: bool,
    ) -> (f64, usize) {
        let ticks = |seconds: f64| (seconds * host_time::ticks_per_second()) as u64;
        let ring = Ring::new(RATE);
        let mut clock = StreamClock::new(RATE, RATE, 960, 312);
        assert!(ring.push_with(960, ticks(100.02), captured(100.0), |_, _| 0.0));
        let publish_next = |ring: &Ring| {
            if mark {
                ring.discontinuity();
            }
            assert!(ring.push_with(960, ticks(101.04), captured(101.02), |_, _| 0.0));
        };
        let mut date = None;
        for at in 0..5 {
            if at == step {
                publish_next(&ring);
            }
            match at {
                0 => {
                    if let Some(start) = ring.take_discontinuity() {
                        assert_eq!(clock.cross(start), Crossing::Continuous);
                    }
                }
                1 => assert_eq!(ring.available(), 960),
                2 => date = Some(ring.tail_time()),
                3 => clock.popping(ring.tail_time(), 960),
                _ => assert_eq!(ring.pop_into(&mut [0.0; 960 * 2]), 960),
            }
        }
        if step == 5 {
            publish_next(&ring);
        }
        if step == 0 {
            assert_eq!(ring.take_discontinuity(), None);
            return (date.unwrap() / host_time::ticks_per_second(), 0);
        }
        let start = ring.take_discontinuity().expect("the next run");
        let Crossing::Gap { silence, packets } = clock.cross(start) else {
            panic!("no gap after step {step}");
        };
        (
            date.unwrap() / host_time::ticks_per_second(),
            silence + packets * 960,
        )
    }

    /// Claude: the controller's ordering, step 2, and every other step.
    #[test]
    fn a_packet_keeps_its_run_date_whenever_the_next_run_is_published() {
        let ticks = |seconds: f64| (seconds * host_time::ticks_per_second()) as u64;
        let (date, _) = first_packet_with_the_next_run_published_after(
            0,
            |s| Captured::Measured(ticks(s)),
            false,
        );
        assert!(
            (date - 101.02).abs() < 1e-6,
            "a run taken first dated {date} s"
        );
        for step in 1..=5 {
            for (label, captured, mark) in [
                ("measured", Captured::Measured as fn(u64) -> Captured, false),
                ("estimated", Captured::Estimated, true),
            ] {
                let (date, gap) = first_packet_with_the_next_run_published_after(
                    step,
                    |s| captured(ticks(s)),
                    mark,
                );
                assert!(
                    (date - 100.0).abs() < 1e-6,
                    "{label}, step {step}: first packet dated {date} s"
                );
                assert_eq!(gap, 48_000, "{label}, step {step}");
            }
        }
    }

    /// Claude: a source that dates nothing is dated by when its pushes arrived,
    /// its own run's pushes only, whenever the next run is published.
    #[test]
    fn an_undated_packet_is_dated_by_its_own_run_arrival_whenever_the_next_run_is_published() {
        for step in 1..=5 {
            let (date, gap) =
                first_packet_with_the_next_run_published_after(step, |_| Captured::Unknown, true);
            assert!(
                (date - 100.0).abs() < 1e-6,
                "step {step}: first packet dated {date} s"
            );
            assert_eq!(gap, 48_000, "step {step}");
        }
    }

    #[test]
    fn a_short_gap_is_filled_with_silence() {
        assert_eq!(
            crossing_after(0.01),
            Crossing::Gap {
                silence: 480,
                packets: 0
            }
        );
    }

    #[test]
    fn a_long_gap_flushes_the_encoder_and_skips_whole_packets() {
        let Crossing::Gap { silence, packets } = crossing_after(1.0) else {
            panic!("not a gap");
        };
        assert_eq!(silence + packets * 960, 48_000);
        assert!(
            silence >= 2 * 960 + 312,
            "{silence} frames do not flush the encoder"
        );
        assert!(silence < 3 * 960 + 312);
    }

    #[test]
    fn a_run_dated_before_where_the_stream_stands_overlaps_it() {
        assert_eq!(crossing_after(-0.005), Crossing::Overlap(240));
    }
}
