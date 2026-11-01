//! Claude: the calibration signal: a linear chirp, played in trains of five
//! at starts whose differences all differ, so that at any wrong alignment of
//! a train at most one probe lines up with another.

use std::f64::consts::PI;

/// Claude: when each probe of a train starts, in seconds from its first.
pub const TRAIN: [f64; 5] = [0.0, 0.5, 1.2, 1.8, 2.6];
/// Claude: from a train's first probe's start to its last probe's end.
pub const TRAIN_SECONDS: f64 = 2.8;
const PROBE_SECONDS: f64 = 0.2;
const LOW_HZ: f64 = 500.0;
const HIGH_HZ: f64 = 6000.0;
const TAPER_SECONDS: f64 = 0.01;
pub const LEVEL: f64 = 0.5;

/// Claude: the probe at `t` seconds from its start: a sweep from `LOW_HZ` to
/// `HIGH_HZ` over `PROBE_SECONDS` with raised-cosine ends, peaking at `LEVEL`,
/// and silence outside it.
pub fn probe_at(t: f64) -> f64 {
    if !(0.0..PROBE_SECONDS).contains(&t) {
        return 0.0;
    }
    let edge = t.min(PROBE_SECONDS - t);
    let taper = if edge < TAPER_SECONDS {
        0.5 * (1.0 - (PI * edge / TAPER_SECONDS).cos())
    } else {
        1.0
    };
    let sweep = (HIGH_HZ - LOW_HZ) / PROBE_SECONDS;
    LEVEL * taper * (2.0 * PI * (LOW_HZ * t + 0.5 * sweep * t * t)).sin()
}

/// Claude: one probe sampled at `rate`.
pub fn probe(rate: f64) -> Vec<f32> {
    let frames = (PROBE_SECONDS * rate).round() as usize;
    (0..frames)
        .map(|frame| probe_at(frame as f64 / rate) as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn train_starts_lie_apart_by_distances_at_least_100_ms_apart() {
        let mut gaps: Vec<f64> = (0..TRAIN.len())
            .flat_map(|a| (a + 1..TRAIN.len()).map(move |b| TRAIN[b] - TRAIN[a]))
            .collect();
        gaps.sort_by(f64::total_cmp);
        for pair in gaps.windows(2) {
            assert!(pair[1] - pair[0] >= 0.1 - 1e-9, "{gaps:?}");
        }
        assert!(TRAIN[TRAIN.len() - 1] + PROBE_SECONDS <= TRAIN_SECONDS + 1e-9);
    }

    #[test]
    fn a_probe_peaks_at_its_level_and_starts_and_ends_silent() {
        for rate in [16_000.0, 44_100.0, 48_000.0, 96_000.0] {
            let probe = probe(rate);
            assert_eq!(probe.len(), (PROBE_SECONDS * rate).round() as usize);
            let peak = probe.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
            assert!((f64::from(peak) - LEVEL).abs() < 0.01, "{rate} Hz: {peak}");
            assert!(probe[0].abs() < 1e-6 && probe[probe.len() - 1].abs() < 1e-3);
        }
    }
}
