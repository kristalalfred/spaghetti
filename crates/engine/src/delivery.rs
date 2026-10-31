use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Claude: a wait the span fell short of grows it this far past that wait, so
/// later stalls up to a quarter longer do not underrun again.
const HEADROOM: f64 = 1.25;
/// Claude: how long the span holds after the latest push that needed it. Over
/// Wi-Fi the large stalls come about a minute apart among smaller ones, and the
/// span must still be there for the next.
const HOLD: f64 = 120.0;
/// Claude: seconds of span shed per second once it stops holding. Routes drain
/// the excess by resampling 0.2 % fast, well inside the playout's 0.5 % correction.
const RELEASE: f64 = 0.002;
/// Claude: the longest span a route buffers. A push later than this is a break
/// in the stream, a stall too long to wait out or a jump in its media time.
const MAX_SPAN: f64 = 1.0;
/// Claude: how fast the fastest transit may rise, so that a sender clock up to
/// 1000 ppm slow against the host clock does not read as ever later delivery.
const DRIFT: f64 = 0.001;
/// Claude: a step in media time between two pushes shorter than this is
/// timestamp rounding, not media that never arrived.
pub const SMALLEST_GAP: f64 = 0.001;

/// Claude: how many frames, at the source's rate, a frame may land in a route's
/// ring after it was captured, beyond the source's fastest delivery: the source's
/// push cadence plus how late its pushes run. A route's playout keeps this many
/// frames more than it needs to render. The source writes it before the push it
/// describes; each route reads it from its render callback.
#[derive(Debug)]
pub struct DeliverySpan(AtomicU64);

impl DeliverySpan {
    pub fn new(frames: f64) -> Self {
        Self(AtomicU64::new(frames.to_bits()))
    }

    pub fn frames(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn publish(&self, frames: f64) {
        self.0.store(frames.to_bits(), Ordering::Relaxed);
    }
}

/// Claude: measures a network source's span from what it knows of each push:
/// the media time its frames cover and when it arrived. A wait longer than the
/// span grows it at once to `HEADROOM` past that wait; its routes have re-primed
/// by then, so the larger target costs them no second gap. A wait the span
/// covers holds it where it is when it comes within `HEADROOM` of the wait the
/// span was sized for, so the span never grows under a playing route. `HOLD`
/// seconds after the latest push that grew or held it, it sheds `RELEASE` per
/// second, never below `jitter`, the lateness the source always allows, plus
/// the push's own length.
pub struct DeliveryMeter {
    span: Arc<DeliverySpan>,
    sample_rate: f64,
    jitter: f64,
    seconds: f64,
    /// Claude: the span as of `held_at`, when the latest push that needed it arrived.
    held: f64,
    held_at: f64,
    latest: Option<Arrival>,
}

struct Arrival {
    at: f64,
    /// Claude: where the push's media ended.
    end: f64,
    /// Claude: the shortest time from the end of a push's media to its arrival.
    fastest: f64,
}

impl DeliveryMeter {
    pub fn new(sample_rate: f64, jitter: f64) -> Self {
        Self {
            span: Arc::new(DeliverySpan::new(jitter * sample_rate)),
            sample_rate,
            jitter,
            seconds: jitter,
            held: jitter,
            held_at: 0.0,
            latest: None,
        }
    }

    pub fn span(&self) -> Arc<DeliverySpan> {
        Arc::clone(&self.span)
    }

    /// Claude: the media between the latest push and one starting at `start`
    /// that never arrived, when the routes' span outlasts it. The source fills it
    /// and records the fill as a late push, so the gap costs its own length
    /// instead of the routes' buffer.
    pub fn gap_before(&self, start: f64) -> Option<Range<f64>> {
        let end = self.latest.as_ref()?.end;
        let gap = start - end;
        (gap > SMALLEST_GAP && gap <= self.seconds).then_some(end..start)
    }

    /// Claude: whether a push starting at `start` breaks from the latest one:
    /// media missing that `gap_before` does not fill, or media going back.
    pub fn breaks_before(&self, start: f64) -> bool {
        let Some(latest) = &self.latest else {
            return false;
        };
        (start - latest.end).abs() > SMALLEST_GAP && self.gap_before(start).is_none()
    }

    /// Claude: a push of the media `media` covers, in seconds, that arrived at
    /// `arrival` seconds. Its first frame is the one that waited longest. Returns
    /// when it would have arrived at the fastest transit; a ring push stamped with
    /// that time shows the routes how far behind its media the source still is,
    /// however a burst is spread out.
    pub fn record(&mut self, media: Range<f64>, arrival: f64) -> f64 {
        let transit = arrival - media.end;
        let fastest = match &self.latest {
            Some(latest) => (latest.fastest + DRIFT * (arrival - latest.at)).min(transit),
            None => transit,
        };
        let wait = arrival - media.start - fastest;
        let (fastest, wait) = if wait > MAX_SPAN {
            (transit, 0.0)
        } else {
            (fastest, wait)
        };
        let floor = self.jitter + (media.end - media.start);
        let holding = (self.held - RELEASE * (arrival - self.held_at - HOLD).max(0.0)).max(floor);
        let (span, needed) = if wait > self.seconds {
            ((wait * HEADROOM).max(floor), true)
        } else {
            (holding, wait * HEADROOM * HEADROOM >= holding)
        };
        let span = span.min(MAX_SPAN);
        if needed {
            self.held = span;
            self.held_at = arrival;
        }
        self.seconds = span;
        self.latest = Some(Arrival {
            at: arrival,
            end: media.end,
            fastest,
        });
        self.span.publish(self.seconds * self.sample_rate);
        media.end + fastest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48000.0;
    const PACKET: f64 = 0.02;
    const TRANSIT: f64 = 0.005;
    const JITTER: f64 = 0.1;

    /// Claude: feeds 20 ms packets of the media from `from` for `seconds`, each
    /// landing at `arrival(end of its media)`.
    fn feed(meter: &mut DeliveryMeter, from: f64, seconds: f64, arrival: impl Fn(f64) -> f64) {
        let packets = (seconds / PACKET).round() as usize;
        for i in 0..packets {
            let start = from + i as f64 * PACKET;
            meter.record(start..start + PACKET, arrival(start + PACKET));
        }
    }

    fn on_time(end: f64) -> f64 {
        end + TRANSIT
    }

    fn span(meter: &DeliveryMeter) -> f64 {
        meter.span().frames() / RATE
    }

    fn assert_at_floor(meter: &DeliveryMeter) {
        let span = span(meter);
        assert!((span - (JITTER + PACKET)).abs() < 1e-9, "span {span}");
    }

    /// Claude: packets due to land in `[start, start + seconds)` land together at its end.
    fn stalled(start: f64, seconds: f64) -> impl Fn(f64) -> f64 {
        move |end| {
            let due = on_time(end);
            if (start..start + seconds).contains(&due) {
                start + seconds
            } else {
                due
            }
        }
    }

    #[test]
    fn steady_delivery_holds_the_floor() {
        let mut meter = DeliveryMeter::new(RATE, JITTER);
        feed(&mut meter, 0.0, 30.0, |end| {
            on_time(end) + 0.004 * (end * 7.0).sin().abs()
        });
        assert_at_floor(&meter);
    }

    #[test]
    fn a_stall_grows_the_span_at_once_and_calm_sheds_it_slowly() {
        let mut meter = DeliveryMeter::new(RATE, JITTER);
        feed(&mut meter, 0.0, 2.0, on_time);
        feed(&mut meter, 2.0, 0.42, stalled(2.0, 0.4));
        let waited = 2.4 - (2.0 - PACKET) - TRANSIT;
        let grown = span(&meter);
        assert!(
            grown >= waited && grown < MAX_SPAN,
            "span {grown} for a wait of {waited}"
        );

        feed(&mut meter, 2.42, 100.0, on_time);
        assert_eq!(span(&meter), grown, "100 s of calm shed the span");

        feed(&mut meter, 102.42, 30.0, on_time);
        let after_calm = span(&meter);
        assert!(
            after_calm >= waited,
            "130 s of calm shed the span to {after_calm}"
        );
        assert!(after_calm < grown, "the span held at {after_calm}");

        feed(&mut meter, 132.42, 300.0, on_time);
        assert_at_floor(&meter);
    }

    #[test]
    fn stalls_the_span_covers_hold_it_without_growing_it() {
        let mut meter = DeliveryMeter::new(RATE, JITTER);
        feed(&mut meter, 0.0, 2.0, on_time);
        feed(&mut meter, 2.0, 1.0, stalled(2.0, 0.36));
        let grown = span(&meter);
        let mut start = 3.0;
        for seconds in [0.4, 0.33, 0.41, 0.38].iter().cycle().take(40) {
            feed(&mut meter, start, 1.5, stalled(start, *seconds));
            start += 1.5;
        }
        assert_eq!(span(&meter), grown);
    }

    #[test]
    fn a_sender_clock_running_slow_is_not_lateness() {
        let mut meter = DeliveryMeter::new(RATE, JITTER);
        feed(&mut meter, 0.0, 600.0, |end| on_time(end * (1.0 + 500e-6)));
        assert_at_floor(&meter);
    }

    #[test]
    fn a_jump_in_media_time_is_not_lateness() {
        let mut meter = DeliveryMeter::new(RATE, JITTER);
        feed(&mut meter, 0.0, 10.0, on_time);
        feed(&mut meter, 0.0, 10.0, |end| on_time(10.0 + end));
        feed(&mut meter, 300.0, 10.0, |end| on_time(end - 280.0));
        assert_at_floor(&meter);
    }

    #[test]
    fn the_span_stops_at_its_cap_and_a_longer_stall_is_a_break() {
        let mut meter = DeliveryMeter::new(RATE, JITTER);
        feed(&mut meter, 0.0, 2.0, on_time);
        feed(&mut meter, 2.0, 0.88, stalled(2.0, 0.9));
        assert_eq!(span(&meter), MAX_SPAN);

        let mut meter = DeliveryMeter::new(RATE, JITTER);
        feed(&mut meter, 0.0, 2.0, on_time);
        feed(&mut meter, 2.0, 4.0, stalled(2.0, 3.0));
        assert_at_floor(&meter);
    }
}
