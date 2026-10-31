//! Claude: the host clock every timestamp in the engine is in: mach absolute
//! time, which Core Audio stamps `AudioTimeStamp::mHostTime` with on macOS
//! and iOS alike. iOS has no `AudioGetCurrentHostTime`, so the engine reads
//! the clock itself.

use std::ops::{Add, Sub};
use std::sync::OnceLock;
use std::time::Duration;

#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
}

fn timebase() -> (u64, u64) {
    static TIMEBASE: OnceLock<(u64, u64)> = OnceLock::new();
    *TIMEBASE.get_or_init(|| {
        let mut info = Timebase { numer: 0, denom: 0 };
        let status = unsafe { mach_timebase_info(&mut info) };
        assert!(
            status == 0 && info.numer > 0 && info.denom > 0,
            "mach_timebase_info failed: {status}"
        );
        (u64::from(info.numer), u64::from(info.denom))
    })
}

/// Claude: the host time now, in ticks.
pub fn now() -> u64 {
    unsafe { mach_absolute_time() }
}

pub fn ticks_per_second() -> f64 {
    let (numer, denom) = timebase();
    1e9 * denom as f64 / numer as f64
}

pub fn to_nanos(ticks: u64) -> u64 {
    let (numer, denom) = timebase();
    (u128::from(ticks) * u128::from(numer) / u128::from(denom)) as u64
}

pub fn from_nanos(nanos: u64) -> u64 {
    let (numer, denom) = timebase();
    (u128::from(nanos) * u128::from(denom) / u128::from(numer)) as u64
}

/// Claude: an instant on the host clock, as Core Audio stamps it in
/// `AudioTimeStamp::mHostTime`. Program inputs and outputs take their
/// capture and playback times in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostTime(u64);

impl HostTime {
    pub fn now() -> Self {
        Self(now())
    }

    pub const fn from_ticks(ticks: u64) -> Self {
        Self(ticks)
    }

    pub const fn ticks(self) -> u64 {
        self.0
    }
}

impl Add<Duration> for HostTime {
    type Output = Self;

    fn add(self, duration: Duration) -> Self {
        Self(
            self.0
                .saturating_add(from_nanos(duration.as_nanos() as u64)),
        )
    }
}

impl Sub<Duration> for HostTime {
    type Output = Self;

    fn sub(self, duration: Duration) -> Self {
        Self(
            self.0
                .saturating_sub(from_nanos(duration.as_nanos() as u64)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nanos_round_trip_through_ticks() {
        let ticks = now();
        let back = from_nanos(to_nanos(ticks));
        assert!(ticks.abs_diff(back) <= 1, "{ticks} came back as {back}");
        let second = from_nanos(1_000_000_000) as f64;
        assert!((second - ticks_per_second()).abs() <= 1.0);
    }

    #[test]
    fn durations_move_host_times_by_their_length() {
        let at = HostTime::from_ticks(1 << 40);
        let later = at + Duration::from_millis(250);
        assert_eq!(later - Duration::from_millis(250), at);
        let ticks = (later.ticks() - at.ticks()) as f64;
        assert!((ticks / ticks_per_second() - 0.25).abs() < 1e-6);
    }
}
