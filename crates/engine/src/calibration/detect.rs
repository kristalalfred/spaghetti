//! Claude: finding a train in a recording. A matched filter turns the
//! recording into the envelope of its correlation with the probe; a train
//! matches where at least four of its five probes peak at their places in the
//! layout. The earliest match that stands clear of the background is the
//! arrival, unless a fainter one comes earlier still, and a match that is no
//! more than a later, stronger match's own sidelobes is no arrival at all.

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use super::ProbeFailure;
use super::probe::{LEVEL, TRAIN, probe};
use super::recorder::Span;

/// Claude: a match's probes stand this far above the background to be
/// detected, and this far to count against a later detected match.
const DETECTED_DB: f64 = 20.0;
const STRUCTURAL_DB: f64 = 10.0;
/// Claude: the least background the detector assumes: what a probe recorded
/// this far below full scale makes of the envelope. Any microphone's own noise
/// lies above it, and a recording silent to the last bit, as a virtual
/// device's loopback is, has no background of its own for faint numerical
/// residue to stand out against.
const FLOOR_DBFS: f64 = -120.0;
/// Claude: an earlier match counts only when it stands this far above what a
/// later, stronger match's sidelobes put there.
const SIDELOBE_MARGIN_DB: f64 = 10.0;
/// Claude: how far a probe's peak may lie from its place in the layout, so
/// the probes of a match agree within twice this.
const PLACE_SECONDS: f64 = 0.0005;
/// Claude: how many probes of a train make a match.
const AGREEING: usize = 4;

fn ratio(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// Claude: where a train sounded, against where it was scheduled to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measured {
    /// Claude: how much later than scheduled the probes found arrived, on
    /// average, in seconds.
    pub residual: f64,
    /// Claude: how far apart their arrivals lay, against their schedules.
    pub spread: f64,
    /// Claude: when the probes found were scheduled to sound, on average, in
    /// host ticks, so it and the residual weigh them alike.
    pub at: f64,
}

pub struct Detector {
    rate: f64,
    template: Vec<f32>,
    /// Claude: the template's spectrum at the latest FFT size it was needed at.
    spectrum: (usize, Vec<Complex<f64>>),
    /// Claude: the envelope of the template's correlation with itself, by
    /// lag in samples, 1 at lag 0.
    autocorrelation: Vec<f64>,
    /// Claude: the envelope a probe recorded at `FLOOR_DBFS` makes.
    floor: f64,
    planner: FftPlanner<f64>,
}

/// Claude: a train's probes as one alignment of the layout finds them: the
/// peak index and envelope of each probe found.
#[derive(Debug, Clone)]
struct Match {
    alignment: usize,
    peaks: [Option<(f64, f64)>; TRAIN.len()],
}

impl Match {
    fn found(&self) -> impl Iterator<Item = (usize, f64, f64)> + '_ {
        self.peaks
            .iter()
            .enumerate()
            .filter_map(|(k, peak)| peak.map(|(index, level)| (k, index, level)))
    }

    /// Claude: whether at least `AGREEING` probes stand `factor` above `floor`.
    fn stands(&self, floor: f64, factor: f64) -> bool {
        self.found()
            .filter(|(_, _, level)| *level >= floor * factor)
            .count()
            >= AGREEING
    }

    fn strength(&self) -> f64 {
        self.found().map(|(_, _, level)| level).sum()
    }
}

impl Detector {
    pub fn new(rate: f64) -> Self {
        let template = probe(rate);
        let mut detector = Self {
            rate,
            spectrum: (0, Vec::new()),
            autocorrelation: Vec::new(),
            floor: 0.0,
            planner: FftPlanner::new(),
            template: template.clone(),
        };
        let mut autocorrelation = detector.envelope(&template);
        autocorrelation.truncate(template.len());
        let peak = autocorrelation[0];
        autocorrelation.iter_mut().for_each(|a| *a /= peak);
        detector.autocorrelation = autocorrelation;
        detector.floor = peak * ratio(FLOOR_DBFS) / LEVEL;
        detector
    }

    /// Claude: the envelope of `signal`'s correlation with the template at
    /// each lag from 0 on.
    fn envelope(&mut self, signal: &[f32]) -> Vec<f64> {
        let size = (signal.len() + self.template.len()).next_power_of_two();
        let forward = self.planner.plan_fft_forward(size);
        if self.spectrum.0 != size {
            self.spectrum = (size, spectrum(&*forward, &self.template, size));
        }
        let inverse = self.planner.plan_fft_inverse(size);
        let mut correlation = spectrum(&*forward, signal, size);
        for (bin, (c, t)) in correlation.iter_mut().zip(&self.spectrum.1).enumerate() {
            let analytic = match bin {
                0 => 1.0,
                b if b == size / 2 => 1.0,
                b if b < size / 2 => 2.0,
                _ => 0.0,
            };
            *c = *c * t.conj() * analytic;
        }
        inverse.process(&mut correlation);
        correlation
            .iter()
            .map(|c| c.norm() / size as f64)
            .take(signal.len())
            .collect()
    }

    /// Claude: where the train scheduled to sound at `sound_at`, in host
    /// ticks for each probe, lies in `span`.
    pub fn measure(&mut self, span: &Span, sound_at: [f64; 5]) -> Result<Measured, ProbeFailure> {
        let samples = &span.samples;
        if samples.len() < self.template.len() {
            return Err(ProbeFailure::NotFound {
                above_background_db: 0.0,
            });
        }
        let mut envelope = self.envelope(samples);
        envelope.truncate(samples.len() - self.template.len() + 1);
        let peak = envelope.iter().copied().fold(0.0, f64::max);
        let mut sorted = envelope.clone();
        let middle = sorted.len() / 2;
        let background = *sorted.select_nth_unstable_by(middle, f64::total_cmp).1;
        let background = background.max(self.floor);

        let matches = self.matches(&envelope, background);
        let arrival = matches
            .iter()
            .filter(|m| m.stands(background, ratio(DETECTED_DB)))
            .filter(|m| !self.is_sidelobe(m, &matches))
            .min_by_key(|m| m.alignment)
            .ok_or_else(|| ProbeFailure::NotFound {
                above_background_db: 20.0 * (peak / background).log10(),
            })?;
        let earlier = matches.iter().any(|m| {
            m.alignment < arrival.alignment
                && m.stands(background, ratio(STRUCTURAL_DB))
                && !self.is_sidelobe(m, &matches)
        });
        if earlier {
            return Err(ProbeFailure::Ambiguous);
        }

        let tick = |index: f64| span.date(index);
        let ticks_per_second = crate::host_time::ticks_per_second();
        let mut probes: Vec<(f64, f64)> = arrival
            .found()
            .map(|(k, index, _)| {
                let late = (tick(refine(&envelope, index)) - sound_at[k]) / ticks_per_second;
                (late, sound_at[k])
            })
            .collect();
        probes.sort_by(|a, b| a.0.total_cmp(&b.0));
        let n = probes.len() as f64;
        Ok(Measured {
            residual: probes.iter().map(|(late, _)| late).sum::<f64>() / n,
            spread: probes[probes.len() - 1].0 - probes[0].0,
            at: probes.iter().map(|(_, at)| at).sum::<f64>() / n,
        })
    }

    /// Claude: every alignment of the layout at which at least `AGREEING`
    /// probes peak `STRUCTURAL_DB` above `background` within `PLACE_SECONDS`
    /// of their places, each peak taken once.
    fn matches(&self, envelope: &[f64], background: f64) -> Vec<Match> {
        let floor = background * ratio(STRUCTURAL_DB);
        let place = (PLACE_SECONDS * self.rate).round() as usize;
        let offsets: Vec<usize> = TRAIN
            .iter()
            .map(|offset| (offset * self.rate).round() as usize)
            .collect();
        let peaks: Vec<usize> = (1..envelope.len().saturating_sub(1))
            .filter(|&i| {
                envelope[i] >= floor
                    && envelope[i] > envelope[i - 1]
                    && envelope[i] >= envelope[i + 1]
            })
            .collect();
        let strongest_near = |at: usize| {
            let from = at.saturating_sub(place);
            let to = (at + place + 1).min(envelope.len());
            let first = peaks.partition_point(|&p| p < from);
            peaks[first..]
                .iter()
                .take_while(|&&p| p < to)
                .copied()
                .max_by(|&a, &b| envelope[a].total_cmp(&envelope[b]))
        };
        let mut alignments: Vec<usize> = peaks
            .iter()
            .flat_map(|&p| offsets.iter().filter_map(move |&o| p.checked_sub(o)))
            .collect();
        alignments.sort_unstable();
        alignments.dedup();
        let mut matches: Vec<Match> = Vec::new();
        for alignment in alignments {
            let mut found = [None; TRAIN.len()];
            for (k, offset) in offsets.iter().enumerate() {
                found[k] = strongest_near(alignment + offset).map(|p| (p as f64, envelope[p]));
            }
            if found.iter().flatten().count() < AGREEING {
                continue;
            }
            let candidate = Match {
                alignment,
                peaks: found,
            };
            match matches.last_mut() {
                Some(last) if last.peaks == candidate.peaks => {}
                Some(last)
                    if alignment - last.alignment <= place
                        && candidate.strength() > last.strength() =>
                {
                    *last = candidate
                }
                Some(last) if alignment - last.alignment <= place => {}
                _ => matches.push(candidate),
            }
        }
        matches
    }

    /// Claude: whether `candidate` is no more than the sidelobes of a later,
    /// stronger match: fewer than `AGREEING` of its probes stand
    /// `SIDELOBE_MARGIN_DB` above what that match's own probes put there.
    fn is_sidelobe(&self, candidate: &Match, matches: &[Match]) -> bool {
        matches
            .iter()
            .filter(|m| m.alignment > candidate.alignment && m.strength() > candidate.strength())
            .any(|stronger| {
                let clear = candidate
                    .found()
                    .filter(|(k, index, level)| {
                        stronger.peaks[*k].is_none_or(|(at, strength)| {
                            let lag = (at - index).round().abs() as usize;
                            let lobe = self.autocorrelation.get(lag).copied().unwrap_or(0.0);
                            *level >= strength * lobe * ratio(SIDELOBE_MARGIN_DB)
                        })
                    })
                    .count();
                clear < AGREEING
            })
    }
}

/// Claude: the peak at `index` placed between samples by the parabola
/// through it and its neighbours.
fn refine(envelope: &[f64], index: f64) -> f64 {
    let i = index as usize;
    if i == 0 || i + 1 >= envelope.len() {
        return index;
    }
    let (before, at, after) = (envelope[i - 1], envelope[i], envelope[i + 1]);
    let curvature = before - 2.0 * at + after;
    if curvature >= 0.0 {
        return index;
    }
    index + 0.5 * (before - after) / curvature
}

/// Claude: the spectrum of `samples`, zero-padded to `size`.
fn spectrum(forward: &dyn Fft<f64>, samples: &[f32], size: usize) -> Vec<Complex<f64>> {
    let mut buffer: Vec<Complex<f64>> = samples
        .iter()
        .map(|&s| Complex::new(f64::from(s), 0.0))
        .chain(std::iter::repeat(Complex::new(0.0, 0.0)))
        .take(size)
        .collect();
    forward.process(&mut buffer);
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::probe::probe_at;
    use crate::host_time;

    /// Claude: a reproducible stream of normal deviates.
    struct Noise(u64);

    impl Noise {
        fn uniform(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }

        fn normal(&mut self) -> f64 {
            let (u, v) = (self.uniform().max(1e-300), self.uniform());
            (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
        }
    }

    const SOUND: f64 = 0.5;

    /// Claude: a 5.3 s recording at `rate` of a train scheduled to sound
    /// `SOUND` s in, arriving along each of `paths` (its delay in seconds and
    /// gain), over noise of `noise` RMS, and when each probe was scheduled.
    fn recorded(rate: f64, paths: &[(f64, f64)], noise: f64) -> (Span, [f64; 5]) {
        let ticks = host_time::ticks_per_second();
        let origin = 1e12;
        let mut deviates = Noise(0x9e37_79b9_7f4a_7c15 ^ rate.to_bits());
        let samples = (0..(5.3 * rate) as usize)
            .map(|i| {
                let t = i as f64 / rate;
                let train: f64 = paths
                    .iter()
                    .flat_map(|(delay, gain)| {
                        TRAIN
                            .iter()
                            .map(move |offset| gain * probe_at(t - SOUND - offset - delay))
                    })
                    .sum();
                (train + noise * deviates.normal()) as f32
            })
            .collect();
        let sound_at = TRAIN.map(|offset| origin + (SOUND + offset) * ticks);
        (Span::linear(samples, origin, ticks / rate), sound_at)
    }

    fn measure(rate: f64, paths: &[(f64, f64)], noise: f64) -> Result<Measured, ProbeFailure> {
        let (span, sound_at) = recorded(rate, paths, noise);
        Detector::new(rate).measure(&span, sound_at)
    }

    #[test]
    fn a_train_is_found_where_it_arrived_to_a_fraction_of_a_sample() {
        for rate in [16_000.0, 44_100.0, 48_000.0, 96_000.0] {
            for frames in [0.0, 0.25, 0.5, 0.73, 4801.4] {
                let delay = 0.0123 + frames / rate;
                let measured = measure(rate, &[(delay, 0.3)], 1e-4).unwrap();
                let error = (measured.residual - delay) * rate;
                assert!(
                    error.abs() < 0.05,
                    "{rate} Hz, {delay} s: {error} frames off"
                );
                assert!(measured.spread * rate < 0.1, "{rate} Hz: {measured:?}");
            }
        }
    }

    /// Claude: how far above the background, in dB, a path of `gain` peaks
    /// over noise of `noise` RMS at 48 kHz: the probe's energy against the
    /// median envelope of filtered noise, 1.177 standard deviations of it.
    fn level_db(gain: f64, noise: f64) -> f64 {
        let energy: f64 = probe(48_000.0).iter().map(|s| f64::from(*s).powi(2)).sum();
        20.0 * (gain * energy / (1.1774 * noise * energy.sqrt())).log10()
    }

    #[test]
    fn a_single_path_shows_no_early_arrival_and_no_ambiguity() {
        for (gain, noise) in [
            (1.0, 1e-6),
            (0.5, 1e-4),
            (0.05, 1e-3),
            (0.01, 3e-3),
            (0.3, 0.02),
        ] {
            let delay = 0.0371;
            let measured = measure(48_000.0, &[(delay, gain)], noise);
            let label = format!("gain {gain}, noise {noise}");
            let measured = measured.unwrap_or_else(|e| panic!("{label}: {e:?}"));
            assert!(
                (measured.residual - delay).abs() * 48_000.0 < 0.2,
                "{label}: {measured:?}"
            );
        }
    }

    #[test]
    fn a_train_that_does_not_stand_20_db_clear_is_not_found() {
        let (gain, noise) = (0.001, 0.01);
        assert!(level_db(gain, noise) < 10.0);
        match measure(48_000.0, &[(0.02, gain)], noise) {
            Err(ProbeFailure::NotFound {
                above_background_db,
            }) => assert!(above_background_db < 20.0, "{above_background_db}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn later_copies_of_the_train_are_not_the_arrival() {
        for echoes in [
            vec![(0.04, 0.8)],
            vec![(0.3, 1.0)],
            vec![(0.007, 1.0), (0.05, 0.9)],
        ] {
            let mut paths = vec![(0.012, 1.0)];
            paths.extend(echoes.iter().map(|(after, gain)| (0.012 + after, *gain)));
            let measured = measure(48_000.0, &paths, 1e-3).unwrap();
            assert!(
                (measured.residual - 0.012).abs() < 1e-5,
                "{echoes:?}: {measured:?}"
            );
        }
    }

    #[test]
    fn a_direct_path_under_a_stronger_echo_is_found_or_ambiguous_never_the_echo() {
        let noise = 1e-3;
        for after in [0.007, 0.02, 0.1] {
            for under_db in [0.0, 6.0, 10.0, 20.0] {
                let direct = 0.5 * 10f64.powf(-under_db / 20.0);
                assert!(level_db(direct, noise) > 30.0);
                let paths = [(0.012, direct), (0.012 + after, 0.5)];
                match measure(48_000.0, &paths, noise) {
                    Ok(measured) => assert!(
                        (measured.residual - 0.012).abs() < 1e-4,
                        "echo {after} s later, direct {under_db} dB under: {measured:?}"
                    ),
                    Err(ProbeFailure::Ambiguous) => {}
                    Err(other) => {
                        panic!("echo {after} s later, direct {under_db} dB under: {other}")
                    }
                }
            }
        }
    }

    #[test]
    fn a_direct_path_heard_short_of_detection_before_an_echo_is_ambiguous() {
        let noise = 0.01;
        let direct = 0.002;
        let level = level_db(direct, noise);
        assert!((12.0..18.0).contains(&level), "{level} dB");
        let paths = [(0.012, direct), (0.112, 0.5)];
        assert_eq!(
            measure(48_000.0, &paths, noise),
            Err(ProbeFailure::Ambiguous)
        );
    }

    #[test]
    fn residue_far_below_any_microphone_in_a_silent_recording_is_no_arrival() {
        let residue = 10f64.powf(-150.0 / 20.0) / 0.5;
        let paths = [(0.012 - 0.188, residue), (0.012, 0.3)];
        let measured = measure(48_000.0, &paths, 0.0).unwrap();
        assert!((measured.residual - 0.012).abs() < 1e-5, "{measured:?}");
    }
}
