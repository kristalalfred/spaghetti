//! Claude: the calibration signal: a linear chirp, played in trains of five
//! at starts whose differences all differ, so that at any wrong alignment of
//! a train at most one probe lines up with another. The reference sweeps up
//! and the output down, each on a layout of its own, so that a train of one
//! heard in the other's recording is neither its probe nor its layout.

use std::f64::consts::PI;

use super::Role;

pub const PROBES: usize = 5;
/// Claude: when each probe of a reference train starts, in seconds from its
/// first.
const REFERENCE_TRAIN: [f64; PROBES] = [0.0, 0.5, 1.2, 1.8, 2.6];
/// Claude: when each probe of an output train starts: the same gaps in
/// another order, so that a reference train at any lag lines up with it on
/// at most two probes, and on two at six lags only.
const OUTPUT_TRAIN: [f64; PROBES] = [0.0, 0.7, 1.5, 2.0, 2.6];
/// Claude: from a train's first probe's start to its last probe's end.
pub const TRAIN_SECONDS: f64 = 2.8;
const PROBE_SECONDS: f64 = 0.2;
const LOW_HZ: f64 = 500.0;
const HIGH_HZ: f64 = 6000.0;
/// Claude: the lowest rate a recording may run at, the lowest common rate
/// above twice `HIGH_HZ`, so that it holds the whole probe unaliased.
pub const MIN_RECORDING_RATE: f64 = 16_000.0;
const TAPER_SECONDS: f64 = 0.01;
/// Claude: -20 dBFS. A built-in microphone beside built-in speakers at high
/// volume clipped on probes at -6 dBFS, where probes stood 79 to 90 dB over
/// the background.
pub const LEVEL: f64 = 0.1;

/// Claude: when each probe of `role`'s trains starts, in seconds from its
/// first.
pub fn layout(role: Role) -> [f64; PROBES] {
    match role {
        Role::Reference => REFERENCE_TRAIN,
        Role::Output => OUTPUT_TRAIN,
    }
}

/// Claude: `role`'s probe at `t` seconds from its start: for the reference a
/// sweep from `LOW_HZ` to `HIGH_HZ` over `PROBE_SECONDS` with raised-cosine
/// ends, peaking at `LEVEL`, for the output the same sweep reversed in time,
/// and silence outside it.
pub fn probe_at(role: Role, t: f64) -> f64 {
    if !(0.0..PROBE_SECONDS).contains(&t) {
        return 0.0;
    }
    match role {
        Role::Reference => sweep(t),
        Role::Output => sweep(PROBE_SECONDS - t),
    }
}

fn sweep(t: f64) -> f64 {
    let edge = t.min(PROBE_SECONDS - t);
    let taper = if edge < TAPER_SECONDS {
        0.5 * (1.0 - (PI * edge / TAPER_SECONDS).cos())
    } else {
        1.0
    };
    let sweep = (HIGH_HZ - LOW_HZ) / PROBE_SECONDS;
    LEVEL * taper * (2.0 * PI * (LOW_HZ * t + 0.5 * sweep * t * t)).sin()
}

/// Claude: `role`'s probe sampled at `rate`.
pub fn probe(rate: f64, role: Role) -> Vec<f32> {
    let frames = (PROBE_SECONDS * rate).round() as usize;
    (0..frames)
        .map(|frame| probe_at(role, frame as f64 / rate) as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLES: [Role; 2] = [Role::Reference, Role::Output];

    #[test]
    fn train_starts_lie_apart_by_distances_at_least_100_ms_apart() {
        for role in ROLES {
            let train = layout(role);
            let mut gaps: Vec<f64> = (0..PROBES)
                .flat_map(|a| (a + 1..PROBES).map(move |b| train[b] - train[a]))
                .collect();
            gaps.sort_by(f64::total_cmp);
            for pair in gaps.windows(2) {
                assert!(pair[1] - pair[0] >= 0.1 - 1e-9, "{role}: {gaps:?}");
            }
            assert!(train[1..].iter().zip(train).all(|(b, a)| b - a >= 0.5));
            assert!(train[PROBES - 1] + PROBE_SECONDS <= TRAIN_SECONDS + 1e-9);
        }
    }

    /// Claude: a reference train at any lag lines up with the output's
    /// layout, within 50 ms, on at most two probes, and on two at six lags
    /// only, among them lag 0, where both trains start and end together.
    #[test]
    fn a_reference_train_at_any_lag_lines_up_with_the_output_layout_on_at_most_two_probes() {
        let (reference, output) = (layout(Role::Reference), layout(Role::Output));
        let lags: Vec<f64> = output
            .iter()
            .flat_map(|o| reference.iter().map(move |r| o - r))
            .collect();
        let lined_up = |lag: f64| {
            lags.iter()
                .filter(|other| (*other - lag).abs() < 0.05)
                .count()
        };
        assert!(lags.iter().all(|&lag| lined_up(lag) <= 2));
        let mut twice: Vec<f64> = lags
            .iter()
            .copied()
            .filter(|&lag| lined_up(lag) == 2)
            .collect();
        twice.sort_by(f64::total_cmp);
        twice.dedup_by(|a, b| (*a - *b).abs() < 0.05);
        assert_eq!(twice.len(), 6, "{twice:?}");
    }

    #[test]
    fn a_probe_peaks_at_its_level_and_starts_and_ends_silent() {
        for role in ROLES {
            for rate in [16_000.0, 44_100.0, 48_000.0, 96_000.0] {
                let probe = probe(rate, role);
                assert_eq!(probe.len(), (PROBE_SECONDS * rate).round() as usize);
                let peak = probe.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
                assert!(
                    (f64::from(peak) - LEVEL).abs() < 0.01,
                    "{role}, {rate} Hz: {peak}"
                );
                assert!(probe[0].abs() < 1e-6 && probe[probe.len() - 1].abs() < 1e-3);
            }
        }
    }

    #[test]
    fn the_output_probe_is_the_reference_probe_reversed() {
        for t in [0.0, 0.003, 0.05, 0.1234, 0.199] {
            let reversed = probe_at(Role::Reference, PROBE_SECONDS - t);
            assert!((probe_at(Role::Output, t) - reversed).abs() < 1e-12, "{t}");
        }
        assert_eq!(probe_at(Role::Output, -0.001), 0.0);
        assert_eq!(probe_at(Role::Output, PROBE_SECONDS), 0.0);
    }
}
