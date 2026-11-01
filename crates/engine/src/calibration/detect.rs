//! Claude: finding a train in a recording. A matched filter turns the
//! recording into the envelope of its correlation with the probe. The
//! strongest train in it gives each probe its strongest peak; each probe's
//! arrival is then the earliest peak heard within `ARRIVAL_DB` of that one,
//! up to `LOOKBACK_SECONDS` before it, that no later, stronger peak's
//! sidelobes explain. So a direct path heard under a later echo is still found, while a
//! speaker's pre-ringing and the probe's sidelobes, which stand further under
//! the arrival, never are. The train is measured where at least four probes'
//! arrivals agree with its layout.

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use super::ProbeFailure;
use super::probe::{LEVEL, TRAIN, probe};
use super::recorder::Span;

/// Claude: the train is heard when at least `AGREEING` probes peak this far
/// above the background, and only a peak this far above it is an arrival or
/// makes a probe ambiguous: noise peaks well under it.
const HEARD_DB: f64 = 20.0;
/// Claude: the least background the detector assumes: what a probe recorded
/// this far below full scale makes of the envelope. Any microphone's own noise
/// lies above it, and a recording silent to the last bit, as a virtual
/// device's loopback is, has no background of its own for faint numerical
/// residue to stand out against.
const FLOOR_DBFS: f64 = -120.0;
/// Claude: an arrival stands at most this far under its probe's strongest peak.
const ARRIVAL_DB: f64 = 15.0;
/// Claude: a peak that stands under the arrival bound but no further than
/// this under the strongest peak, and more than `RINGING_SECONDS` before the
/// arrival, could be a direct path heard just too faintly, so the probe is
/// ambiguous rather than measured on the later path.
const AMBIGUITY_DB: f64 = 21.0;
/// Claude: how far ahead of its arrival a speaker's own response may sound:
/// measured, built-in speakers ring up to 2.8 ms ahead, 27 to 32 dB under,
/// and an AirPlay soundbar sounds 19 dB under 2.6 ms ahead of its first
/// arrival. Paths 4 ms apart differ by 1.4 m, more than inside one speaker.
const RINGING_SECONDS: f64 = 0.004;
/// Claude: a peak no more than this above what a later, stronger peak's
/// sidelobes put there is one of them.
const SIDELOBE_MARGIN_DB: f64 = 6.0;
/// Claude: how far in lag a recorded sidelobe may stray from the template's:
/// a span about half the sidelobes' spacing, so the template's sidelobes are
/// taken by their upper envelope, and a speaker that reshapes them a little
/// is allowed for.
const SIDELOBE_SLACK_SECONDS: f64 = 0.0001;
/// Claude: how far before its probe's strongest peak an arrival may lie. The
/// previous probe of a train starts at least 0.5 s earlier.
const LOOKBACK_SECONDS: f64 = 0.35;
/// Claude: how far from its place in the layout a probe's strongest peak may
/// lie, wide enough for a cluster of near-equal arrivals and the drift of a
/// train.
const NEAR_SECONDS: f64 = 0.003;
/// Claude: how far the arrivals of the probes of a train may stray from one
/// another's places in the layout.
const AGREEMENT_SECONDS: f64 = 0.001;
/// Claude: how many probes of a train make a match.
const AGREEING: usize = 4;

fn ratio(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// Claude: where a train sounded, against where it was scheduled to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measured {
    /// Claude: how much later than scheduled the agreeing probes arrived, on
    /// average, in seconds.
    pub residual: f64,
    /// Claude: how far apart their arrivals lay, against their schedules.
    pub spread: f64,
    /// Claude: when the agreeing probes were scheduled to sound, on average,
    /// in host ticks, so it and the residual weigh them alike.
    pub at: f64,
}

pub struct Detector {
    rate: f64,
    template: Vec<f32>,
    /// Claude: the template's spectrum at the latest FFT size it was needed at.
    spectrum: (usize, Vec<Complex<f64>>),
    /// Claude: the upper envelope of the template's correlation with itself,
    /// by lag in samples, 1 at lag 0.
    sidelobes: Vec<f64>,
    /// Claude: the envelope a probe recorded at `FLOOR_DBFS` makes.
    floor: f64,
    planner: FftPlanner<f64>,
}

impl Detector {
    pub fn new(rate: f64) -> Self {
        let template = probe(rate);
        let mut detector = Self {
            rate,
            spectrum: (0, Vec::new()),
            sidelobes: Vec::new(),
            floor: 0.0,
            planner: FftPlanner::new(),
            template: template.clone(),
        };
        let mut autocorrelation = detector.envelope(&template);
        autocorrelation.truncate(template.len());
        let peak = autocorrelation[0];
        let slack = (SIDELOBE_SLACK_SECONDS * rate).round() as usize;
        detector.sidelobes = (0..autocorrelation.len())
            .map(|lag| {
                let around =
                    lag.saturating_sub(slack)..(lag + slack + 1).min(autocorrelation.len());
                autocorrelation[around].iter().copied().fold(0.0, f64::max) / peak
            })
            .collect();
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
        let mut sorted = envelope.clone();
        let middle = sorted.len() / 2;
        let background = *sorted.select_nth_unstable_by(middle, f64::total_cmp).1;
        let background = background.max(self.floor);

        let strongest = self.strongest_train(&envelope);
        let heard: Vec<(usize, usize)> = strongest
            .iter()
            .enumerate()
            .filter(|(_, peak)| envelope[**peak] >= background * ratio(HEARD_DB))
            .map(|(k, peak)| (k, *peak))
            .collect();
        if heard.len() < AGREEING {
            let loudest = strongest.iter().map(|&p| envelope[p]).fold(0.0, f64::max);
            return Err(ProbeFailure::NotFound {
                above_background_db: 20.0 * (loudest / background).log10(),
            });
        }

        let offset = |k: usize| (TRAIN[k] * self.rate).round();
        let mut arrivals: Vec<(usize, f64)> = heard
            .iter()
            .filter_map(|&(k, peak)| {
                let arrival = self.arrival(&envelope, peak, background)?;
                Some((k, arrival as f64 - offset(k)))
            })
            .collect();
        arrivals.sort_by(|a, b| a.1.total_cmp(&b.1));
        let agreement = AGREEMENT_SECONDS * self.rate;
        let agreeing = (0..arrivals.len())
            .map(|first| {
                let count = arrivals[first..]
                    .iter()
                    .take_while(|(_, at)| at - arrivals[first].1 <= agreement)
                    .count();
                &arrivals[first..first + count]
            })
            .max_by_key(|agreeing| agreeing.len())
            .unwrap_or(&[]);
        if agreeing.len() < AGREEING {
            return Err(ProbeFailure::Ambiguous);
        }

        let ticks_per_second = crate::host_time::ticks_per_second();
        let mut probes: Vec<(f64, f64)> = agreeing
            .iter()
            .map(|&(k, at)| {
                let index = refine(&envelope, at + offset(k));
                (
                    (span.date(index) - sound_at[k]) / ticks_per_second,
                    sound_at[k],
                )
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

    /// Claude: each probe's strongest peak where the train is strongest: the
    /// alignment of the layout whose probes' loudest envelope within
    /// `NEAR_SECONDS` of their places sums highest.
    fn strongest_train(&self, envelope: &[f64]) -> [usize; TRAIN.len()] {
        let block = ((NEAR_SECONDS * self.rate) as usize).max(1);
        let blocks: Vec<f64> = envelope
            .chunks(block)
            .map(|chunk| chunk.iter().copied().fold(0.0, f64::max))
            .collect();
        let places = TRAIN.map(|offset| (offset * self.rate) as usize / block);
        let near = |at: usize| at.saturating_sub(1)..(at + 2).min(blocks.len());
        let score = |alignment: usize| -> f64 {
            places
                .iter()
                .map(|place| {
                    near(alignment + place)
                        .map(|b| blocks[b])
                        .fold(0.0, f64::max)
                })
                .sum()
        };
        let span = blocks.len().saturating_sub(places[TRAIN.len() - 1]);
        let best = (0..span.max(1))
            .max_by(|&a, &b| score(a).total_cmp(&score(b)))
            .unwrap_or(0);
        places.map(|place| {
            let around = near(best + place);
            let from = around.start * block;
            let to = (around.end * block).min(envelope.len());
            (from..to.max(from + 1).min(envelope.len()))
                .max_by(|&a, &b| envelope[a].total_cmp(&envelope[b]))
                .unwrap_or(from.min(envelope.len() - 1))
        })
    }

    /// Claude: the earliest peak within `ARRIVAL_DB` of `strongest`, at most
    /// `LOOKBACK_SECONDS` before it, that no later peak's sidelobes explain;
    /// None when a peak in the band down to `AMBIGUITY_DB` comes more than
    /// `RINGING_SECONDS` before that.
    fn arrival(&self, envelope: &[f64], strongest: usize, background: f64) -> Option<usize> {
        let level = envelope[strongest];
        let from = strongest
            .saturating_sub((LOOKBACK_SECONDS * self.rate) as usize)
            .max(1);
        let candidates: Vec<usize> = (from..strongest)
            .filter(|&i| {
                is_peak(envelope, i)
                    && envelope[i] >= level * ratio(-AMBIGUITY_DB)
                    && envelope[i] >= background * ratio(HEARD_DB)
                    && !self.is_sidelobe(envelope, i)
            })
            .collect();
        let arrival = candidates
            .iter()
            .copied()
            .find(|&i| envelope[i] >= level * ratio(-ARRIVAL_DB))
            .unwrap_or(strongest);
        let ringing = (RINGING_SECONDS * self.rate) as usize;
        match candidates.first() {
            Some(&earliest) if earliest + ringing < arrival => None,
            _ => Some(arrival),
        }
    }

    /// Claude: whether a later, stronger peak's sidelobes explain the peak at
    /// `i`.
    fn is_sidelobe(&self, envelope: &[f64], i: usize) -> bool {
        let end = (i + self.sidelobes.len()).min(envelope.len() - 1);
        (i + 1..end).any(|j| {
            envelope[j] > envelope[i]
                && is_peak(envelope, j)
                && envelope[j] * self.sidelobes[j - i] * ratio(SIDELOBE_MARGIN_DB) >= envelope[i]
        })
    }
}

fn is_peak(envelope: &[f64], i: usize) -> bool {
    envelope[i] > envelope[i - 1] && envelope[i] >= envelope[i + 1]
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

    /// Claude: a response: each path's delay in seconds and its gain, heard
    /// on every probe or only on those `probes` names.
    #[derive(Clone)]
    struct Path {
        delay: f64,
        gain: f64,
        probes: Option<Vec<usize>>,
    }

    fn paths(taps: &[(f64, f64)]) -> Vec<Path> {
        taps.iter()
            .map(|&(delay, gain)| Path {
                delay,
                gain,
                probes: None,
            })
            .collect()
    }

    /// Claude: a 5.3 s recording at `rate` of a train scheduled to sound
    /// `SOUND` s in, arriving along each of `paths`, over noise of `noise`
    /// RMS, and when each probe was scheduled.
    fn recorded(rate: f64, paths: &[Path], noise: f64) -> (Span, [f64; 5]) {
        let ticks = host_time::ticks_per_second();
        let origin = 1e12;
        let mut deviates = Noise(0x9e37_79b9_7f4a_7c15 ^ rate.to_bits());
        let samples = (0..(5.3 * rate) as usize)
            .map(|i| {
                let t = i as f64 / rate;
                let train: f64 = paths
                    .iter()
                    .flat_map(|path| {
                        TRAIN.iter().enumerate().map(move |(k, offset)| {
                            let heard = path.probes.as_ref().is_none_or(|on| on.contains(&k));
                            if heard {
                                path.gain * probe_at(t - SOUND - offset - path.delay)
                            } else {
                                0.0
                            }
                        })
                    })
                    .sum();
                (train + noise * deviates.normal()) as f32
            })
            .collect();
        let sound_at = TRAIN.map(|offset| origin + (SOUND + offset) * ticks);
        (Span::linear(samples, origin, ticks / rate), sound_at)
    }

    fn measure(rate: f64, taps: &[(f64, f64)], noise: f64) -> Result<Measured, ProbeFailure> {
        let (span, sound_at) = recorded(rate, &paths(taps), noise);
        Detector::new(rate).measure(&span, sound_at)
    }

    fn db(db: f64) -> f64 {
        10f64.powf(db / 20.0)
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
        let template: Vec<f32> = probe(48_000.0);
        let energy: f64 = template.iter().map(|s| f64::from(*s).powi(2)).sum();
        let peak = gain / LEVEL * energy;
        20.0 * (peak / (1.1774 * noise * energy.sqrt())).log10()
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
            let label = format!("gain {gain}, noise {noise}");
            let measured = measure(48_000.0, &[(delay, gain)], noise)
                .unwrap_or_else(|e| panic!("{label}: {e:?}"));
            assert!(
                (measured.residual - delay).abs() * 48_000.0 < 0.2,
                "{label}: {measured:?}"
            );
        }
    }

    #[test]
    fn a_train_that_does_not_stand_20_db_clear_is_not_found() {
        let (gain, noise) = (0.0002, 0.01);
        assert!(level_db(gain, noise) < 10.0, "{}", level_db(gain, noise));
        match measure(48_000.0, &[(0.02, gain)], noise) {
            Err(ProbeFailure::NotFound {
                above_background_db,
            }) => assert!(above_background_db < 20.0, "{above_background_db}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn later_copies_of_the_train_within_the_bound_leave_the_direct_path_the_arrival() {
        for echoes in [
            vec![(0.04, 0.8)],
            vec![(0.3, 1.0)],
            vec![(0.007, 1.0), (0.05, 0.9)],
        ] {
            let mut taps = vec![(0.012, 1.0)];
            taps.extend(echoes.iter().map(|(after, gain)| (0.012 + after, *gain)));
            let measured = measure(48_000.0, &taps, 1e-3).unwrap();
            assert!(
                (measured.residual - 0.012).abs() < 1e-5,
                "{echoes:?}: {measured:?}"
            );
        }
    }

    /// Claude: a direct path just inside the arrival bound under a later
    /// echo is the arrival, one in the band below it is ambiguous, and one
    /// further under leaves the echo the arrival.
    #[test]
    fn a_direct_path_under_an_echo_is_found_ambiguous_or_lost_by_how_far_under() {
        for after in [0.02, 0.1] {
            for (under_db, expected) in [
                (0.0, Some(0.0)),
                (10.0, Some(0.0)),
                (14.5, Some(0.0)),
                (15.5, None),
                (18.0, None),
                (20.5, None),
                (21.5, Some(after)),
                (30.0, Some(after)),
            ] {
                let taps = [(0.012, 0.3 * db(-under_db)), (0.012 + after, 0.3)];
                let label = format!("echo {after} s later, direct {under_db} dB under");
                match (measure(48_000.0, &taps, 1e-5), expected) {
                    (Ok(measured), Some(late)) => assert!(
                        (measured.residual - 0.012 - late).abs() < 2e-5,
                        "{label}: {measured:?}"
                    ),
                    (Err(ProbeFailure::Ambiguous), None) => {}
                    (result, _) => panic!("{label}: {result:?}"),
                }
            }
        }
    }

    #[test]
    fn a_direct_path_far_under_a_close_echo_aligns_to_the_echo() {
        let taps = [(0.012, 0.3 * db(-30.0)), (0.015, 0.3)];
        let measured = measure(48_000.0, &taps, 1e-4).unwrap();
        assert!((measured.residual - 0.012).abs() <= 0.0035, "{measured:?}");
    }

    #[test]
    fn a_near_equal_earlier_arrival_on_some_probes_only_is_ambiguous() {
        let mut taps = paths(&[(0.012, 0.3)]);
        taps.push(Path {
            delay: 0.002,
            gain: 0.29,
            probes: Some(vec![0, 2]),
        });
        let (span, sound_at) = recorded(48_000.0, &taps, 1e-4);
        assert_eq!(
            Detector::new(48_000.0).measure(&span, sound_at),
            Err(ProbeFailure::Ambiguous)
        );
        taps[1].probes = Some(vec![3]);
        let (span, sound_at) = recorded(48_000.0, &taps, 1e-4);
        let measured = Detector::new(48_000.0).measure(&span, sound_at).unwrap();
        assert!((measured.residual - 0.012).abs() < 1e-5, "{measured:?}");
    }

    /// Claude: a speaker whose response rings ahead of its arrival, 27 to 32
    /// dB under it, within 3 ms: the main path is the arrival.
    #[test]
    fn pre_ringing_under_the_band_is_neither_an_arrival_nor_ambiguous() {
        let taps = [
            (0.020, 0.3),
            (0.020 - 0.0012, 0.3 * db(-29.0)),
            (0.020 - 0.0026, 0.3 * db(-31.0)),
            (0.020 - 0.0018, 0.3 * db(-27.5)),
        ];
        let measured = measure(48_000.0, &taps, 1e-5).unwrap();
        assert!((measured.residual - 0.020).abs() < 1e-5, "{measured:?}");
    }

    /// Claude: a speaker with several drivers: near-equal arrivals within
    /// 2 ms, then strong reflections 14 and 20 ms after the first. The first
    /// arrival is the arrival.
    #[test]
    fn a_cluster_of_near_equal_arrivals_is_measured_at_its_first() {
        let first = 0.031;
        let taps = [
            (first, 0.25),
            (first + 0.0005, 0.25 * db(-0.6)),
            (first + 0.00125, 0.25 * db(-0.4)),
            (first + 0.00175, 0.26),
            (first + 0.014, 0.25 * db(-3.0)),
            (first + 0.020, 0.25 * db(-6.0)),
        ];
        let measured = measure(48_000.0, &taps, 1e-5).unwrap();
        assert!((measured.residual - first).abs() < 5e-5, "{measured:?}");
    }

    #[test]
    fn residue_far_below_any_microphone_in_a_silent_recording_is_no_arrival() {
        let residue = 10f64.powf(-150.0 / 20.0) / LEVEL;
        let measured = measure(48_000.0, &[(0.012 - 0.188, residue), (0.012, 0.3)], 0.0).unwrap();
        assert!((measured.residual - 0.012).abs() < 1e-5, "{measured:?}");
    }

    /// Claude: speakers built as measured ones sound, each found at its
    /// first arrival on every probe without ambiguity: built-in speakers that
    /// ring ahead of a sharp arrival, 27 to 32 dB under it within 3 ms, and a
    /// soundbar whose drivers arrive near-equal over 2 ms, the strongest of
    /// them changing from probe to probe, reflected 14 and 20 ms after its
    /// strongest, and sounding 19 dB under that 2.6 ms ahead of its first
    /// arrival: 4.6 ms ahead of the strongest on some probes.
    #[test]
    fn speakers_shaped_as_measured_are_found_at_their_first_arrival() {
        let built_in = 0.000_35;
        let speakers = paths(&[
            (built_in, 0.3),
            (built_in - 0.001_75, 0.3 * db(-32.0)),
            (built_in - 0.002_77, 0.3 * db(-31.0)),
        ]);
        let first = -0.040_08;
        let strongest = 0.25 * db(1.0);
        let cluster = [
            (0.0, 0.0),
            (0.000_6, -0.3),
            (0.001_3, -0.2),
            (0.002_0, -0.4),
        ];
        let mut soundbar = Vec::new();
        for (probes, strongest_after) in [(vec![0, 2, 4], 0.002_0), (vec![1, 3], 0.000_6)] {
            let mut on = |delay: f64, gain: f64| {
                soundbar.push(Path {
                    delay,
                    gain,
                    probes: Some(probes.clone()),
                })
            };
            for (after, under_db) in cluster {
                let gain = if after == strongest_after {
                    strongest
                } else {
                    0.25 * db(under_db)
                };
                on(first + after, gain);
            }
            on(first - 0.002_6, strongest * db(-19.0));
            on(first + strongest_after + 0.014, strongest * db(-3.0));
            on(first + strongest_after + 0.020, strongest * db(-6.0));
        }
        for (label, paths, arrival) in [
            ("built-in", speakers, built_in),
            ("soundbar", soundbar, first),
        ] {
            let (span, sound_at) = recorded(48_000.0, &paths, 1e-5);
            let measured = Detector::new(48_000.0)
                .measure(&span, sound_at)
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert!(
                (measured.residual - arrival).abs() < 5e-5 && measured.spread < 1e-4,
                "{label}: {measured:?}"
            );
        }
    }
}
