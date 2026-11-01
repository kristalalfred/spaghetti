use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{FRAMES_PER_PACKET, SAMPLE_RATE};

const NANOS_PER_SECOND: u128 = 1_000_000_000;
const UNIX_TO_NTP_SECONDS: u64 = 2_208_988_800;

/// Claude: an NTP timestamp: seconds in the upper 32 bits, the fraction of a
/// second in the lower 32.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Ntp(pub(crate) u64);

impl Ntp {
    /// Claude: how much later than `earlier` this is, in milliseconds.
    pub(crate) fn ms_after(self, earlier: Ntp) -> f64 {
        self.0.wrapping_sub(earlier.0) as i64 as f64 * 1e3 / (1u64 << 32) as f64
    }

    pub(crate) fn to_be_bytes(self) -> [u8; 8] {
        self.0.to_be_bytes()
    }

    pub(crate) fn from_be_bytes(bytes: [u8; 8]) -> Ntp {
        Ntp(u64::from_be_bytes(bytes))
    }
}

/// Claude: seconds since the NTP epoch, to the microsecond, as the debug log
/// shows them.
impl std::fmt::Display for Ntp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let micros = ((self.0 & 0xFFFF_FFFF) * 1_000_000) >> 32;
        write!(f, "{}.{micros:06}", self.0 >> 32)
    }
}

fn ntp_ticks(duration: Duration) -> u64 {
    (((duration.as_nanos() << 32) + NANOS_PER_SECOND / 2) / NANOS_PER_SECOND) as u64
}

#[cfg(test)]
fn ntp_duration(ticks: u64) -> Duration {
    let nanos = (u128::from(ticks) * NANOS_PER_SECOND + (1 << 31)) >> 32;
    Duration::from_nanos(nanos as u64)
}

/// Claude: a stream's clock as the receiver sees it: `Instant`s as NTP
/// timestamps, from the wall clock's reading when the stream started.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Clock {
    base: Instant,
    base_ntp: Ntp,
}

impl Clock {
    pub(crate) fn start() -> Clock {
        let since_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Clock {
            base: Instant::now(),
            base_ntp: Ntp(ntp_ticks(since_unix) + (UNIX_TO_NTP_SECONDS << 32)),
        }
    }

    pub(crate) fn ntp(&self, at: Instant) -> Ntp {
        match at.checked_duration_since(self.base) {
            Some(after) => Ntp(self.base_ntp.0.wrapping_add(ntp_ticks(after))),
            None => Ntp(self.base_ntp.0.wrapping_sub(ntp_ticks(self.base - at))),
        }
    }

    pub(crate) fn now(&self) -> Ntp {
        self.ntp(Instant::now())
    }

    #[cfg(test)]
    pub(crate) fn instant(&self, ntp: Ntp) -> Instant {
        let ticks = ntp.0.wrapping_sub(self.base_ntp.0) as i64;
        if ticks >= 0 {
            self.base + ntp_duration(ticks as u64)
        } else {
            self.base - ntp_duration(ticks.unsigned_abs())
        }
    }
}

/// Claude: where a stream's packets sit in time and on the RTP timeline. Frame
/// 0 is the first frame of packet 0, which plays at `anchor`; frames before it
/// have negative indices.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timeline {
    pub(crate) anchor: Instant,
    pub(crate) first_seq: u16,
    pub(crate) first_rtp: u32,
}

impl Timeline {
    pub(crate) fn play_at(&self, packet: u64) -> Instant {
        self.frame_at(packet as i64 * FRAMES_PER_PACKET as i64)
    }

    pub(crate) fn frame_at(&self, frame: i64) -> Instant {
        let nanos = |frames: u64| {
            Duration::from_nanos(
                (u128::from(frames) * NANOS_PER_SECOND / u128::from(SAMPLE_RATE)) as u64,
            )
        };
        if frame >= 0 {
            self.anchor + nanos(frame as u64)
        } else {
            self.anchor - nanos(frame.unsigned_abs())
        }
    }

    /// Claude: the last frame whose play time is at or before `at`.
    pub(crate) fn frame_playing(&self, at: Instant) -> i64 {
        let frames = |d: Duration| d.as_nanos() * u128::from(SAMPLE_RATE) / NANOS_PER_SECOND;
        let mut frame = match at.checked_duration_since(self.anchor) {
            Some(after) => frames(after) as i64,
            None => -(frames(self.anchor - at) as i64),
        };
        while self.frame_at(frame) > at {
            frame -= 1;
        }
        while self.frame_at(frame + 1) <= at {
            frame += 1;
        }
        frame
    }

    pub(crate) fn seq(&self, packet: u64) -> u16 {
        self.first_seq.wrapping_add(packet as u16)
    }

    pub(crate) fn rtp(&self, frame: i64) -> u32 {
        self.first_rtp.wrapping_add(frame as u32)
    }
}

pub(crate) fn frames(duration: Duration) -> u32 {
    (duration.as_nanos() * u128::from(SAMPLE_RATE) / NANOS_PER_SECOND) as u32
}

pub(crate) const PACKET_DURATION: Duration =
    Duration::from_nanos(FRAMES_PER_PACKET as u64 * 1_000_000_000 / SAMPLE_RATE as u64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntp_times_show_as_seconds_to_the_microsecond_and_differ_in_milliseconds() {
        let later = Ntp((3_900_000_000 << 32) | (1 << 31));
        assert_eq!(later.to_string(), "3900000000.500000");
        let earlier = Ntp(3_900_000_000 << 32);
        assert_eq!(later.ms_after(earlier), 500.0);
        assert_eq!(earlier.ms_after(later), -500.0);
    }

    #[test]
    fn ntp_keeps_whole_seconds_above_the_fraction() {
        assert_eq!(ntp_ticks(Duration::from_secs(3)), 3 << 32);
        assert_eq!(ntp_ticks(Duration::from_millis(500)), 1 << 31);
        assert_eq!(
            ntp_ticks(Duration::from_millis(1250)),
            (1 << 32) + (1 << 30)
        );
        assert_eq!(
            ntp_duration(ntp_ticks(Duration::from_nanos(123_456_789))),
            Duration::from_nanos(123_456_789)
        );
    }

    #[test]
    fn the_clock_reads_the_wall_clock_at_its_base_and_maps_instants_both_ways() {
        let clock = Clock::start();
        let unix_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ntp_seconds = clock.now().0 >> 32;
        assert!(ntp_seconds.abs_diff(unix_seconds + UNIX_TO_NTP_SECONDS) <= 1);

        let later = clock.base + Duration::from_millis(1500);
        assert_eq!(clock.ntp(later).0 - clock.base_ntp.0, (1 << 32) + (1 << 31));
        let earlier = clock.base - Duration::from_millis(250);
        assert_eq!(clock.base_ntp.0 - clock.ntp(earlier).0, 1 << 30);
        assert_eq!(clock.instant(clock.ntp(later)), later);
        assert_eq!(clock.instant(clock.ntp(earlier)), earlier);
    }

    #[test]
    fn packets_play_at_exact_frame_times_without_drift() {
        let anchor = Instant::now();
        let timeline = Timeline {
            anchor,
            first_seq: 0,
            first_rtp: 0,
        };
        assert_eq!(timeline.play_at(0), anchor);
        assert_eq!(
            timeline.play_at(1) - anchor,
            Duration::from_nanos(7_981_859)
        );
        assert_eq!(timeline.play_at(44_100) - anchor, Duration::from_secs(352));
        assert_eq!(anchor - timeline.frame_at(-44_100), Duration::from_secs(1));
    }

    #[test]
    fn the_frame_playing_is_the_last_one_started() {
        let anchor = Instant::now() + Duration::from_secs(10);
        let timeline = Timeline {
            anchor,
            first_seq: 0,
            first_rtp: 0,
        };
        for frame in [-88_200, -1, 0, 1, 351, 352, 1_000_000] {
            let at = timeline.frame_at(frame);
            assert_eq!(timeline.frame_playing(at), frame);
            assert_eq!(timeline.frame_playing(at + Duration::from_nanos(1)), frame);
            assert_eq!(
                timeline.frame_playing(at - Duration::from_nanos(1)),
                frame - 1
            );
        }
    }

    #[test]
    fn sequence_numbers_and_rtp_times_wrap() {
        let timeline = Timeline {
            anchor: Instant::now(),
            first_seq: 65_535,
            first_rtp: u32::MAX - 351,
        };
        assert_eq!(timeline.seq(1), 0);
        assert_eq!(timeline.rtp(352), 0);
        assert_eq!(timeline.rtp(-1), u32::MAX - 352);
    }
}
