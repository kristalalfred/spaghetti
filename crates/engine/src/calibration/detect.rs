//! Claude: finding a train in a recording. A matched filter turns the recording
//! into the envelope of its correlation with the train's role's probe, which
//! answers the other role's probe 32.6 dB under its own, smeared over a third
//! of a second rather than a peak. Each alignment of the train's layout that
//! gives its probes their strongest peaks is a candidate. One with `AGREEING`
//! probes heard is significant within `ARRIVAL_DB` of the loudest such, and a
//! group, one path of the train, when its probes, each arriving where its own
//! cluster of peaks starts, agree with the layout. A train is measured only
//! when every significant candidate is a group. Otherwise the loudest that is
//! not fails it by its own arrivals, as `Moved` or `Disagree`, so that no
//! quieter train or later echo stands in for a louder sound left unexplained.
//! The train arrives with the earliest significant group, and an earlier group
//! up to `AMBIGUITY_DB` under the loudest makes it ambiguous; no group under
//! the significant ones removes or excludes anything. Each probe's arrival is
//! then the earliest peak heard within `ARRIVAL_DB` of the arriving group's, up
//! to `LOOKBACK_SECONDS` before it, that no later, stronger peak's sidelobes
//! explain and that is not where a significant group has another of its probes.
//! So a direct path heard under a later echo is still found, while a speaker's
//! pre-ringing and the probe's sidelobes, which stand further under the
//! arrival, never are. The train is measured where at least four probes'
//! arrivals agree with its layout; with no candidate heard, the best-ranked is
//! measured, to say why it fails.

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use tracing::{Level, debug};

use super::probe::{LEVEL, PROBES, layout, probe};
use super::recorder::Span;
use super::{ProbeFailure, Role};

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
/// Claude: a probe's strongest peak is sought from this far before its place
/// in the layout to twice this far after it, wide enough for a cluster of
/// near-equal arrivals and the drift of a train.
const NEAR_SECONDS: f64 = 0.003;
/// Claude: how far the arrivals of the probes of a train may stray from one
/// another's places in the layout.
const AGREEMENT_SECONDS: f64 = 0.001;
/// Claude: how many probes of a train make a match.
const AGREEING: usize = 4;
/// Claude: how many standard errors, against the arrivals' scatter about
/// their line, a train's slope must stand from zero for its timing to have
/// moved rather than its arrivals to have scattered: past the 99% two-sided
/// t value for four arrivals, 9.9, and for five, 5.8.
const TREND: f64 = 10.0;

fn ratio(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

fn db(ratio: f64) -> f64 {
    20.0 * ratio.log10()
}

/// Claude: a train candidate whose probes agree: where in the envelope its
/// first probe arrived by the median of their arrivals, the candidate's
/// level, which probes agree, and the candidate.
struct Group {
    at: f64,
    level: f64,
    probes: Vec<usize>,
    candidate: usize,
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// Claude: what finding one probe's arrival came to, as the debug log shows it.
struct Found {
    /// Claude: the earliest peak within `ARRIVAL_DB` of the strongest that no
    /// sidelobes explain, or the strongest: the arrival unless a near miss
    /// makes the probe ambiguous.
    within: usize,
    /// Claude: the peaks in the band down to `AMBIGUITY_DB` that came more
    /// than `RINGING_SECONDS` before `within`.
    near_misses: Vec<usize>,
    /// Claude: the peaks a later, stronger peak's sidelobes explain, kept
    /// only while the debug log is on.
    sidelobes: Vec<usize>,
}

impl Found {
    fn arrival(&self) -> Option<usize> {
        self.near_misses.is_empty().then_some(self.within)
    }
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
    /// Claude: how much later each second the agreeing probes arrived against
    /// their schedules, by the line through them, in seconds a second.
    pub drift: f64,
}

/// Claude: what finds one role's trains: its probe as the template, its
/// layout, and the sidelobes of its probe's correlation with itself.
pub struct Detector {
    rate: f64,
    layout: [f64; PROBES],
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
    pub fn new(rate: f64, role: Role) -> Self {
        let template = probe(rate, role);
        let mut detector = Self {
            rate,
            layout: layout(role),
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
                heard: 0,
                required: AGREEING,
                above_background_db: 0.0,
            });
        }
        let mut envelope = self.envelope(samples);
        envelope.truncate(samples.len() - self.template.len() + 1);
        let mut sorted = envelope.clone();
        let middle = sorted.len() / 2;
        let background = *sorted.select_nth_unstable_by(middle, f64::total_cmp).1;
        let background = background.max(self.floor);

        let candidates = self.candidates(&envelope, background);
        let levels: Vec<Option<f64>> = candidates
            .iter()
            .map(|strongest| self.heard_level(&envelope, background, strongest))
            .collect();
        let Some(loudest) = levels.iter().flatten().copied().reduce(f64::max) else {
            return self.measure_train(
                span,
                sound_at,
                &envelope,
                background,
                &[],
                (0, candidates[0]),
            );
        };
        let significant = loudest * ratio(-ARRIVAL_DB);
        let mut groups: Vec<Group> = Vec::new();
        let mut unexplained: Option<(usize, f64)> = None;
        for (candidate, strongest) in candidates.iter().enumerate() {
            let Some(level) = levels[candidate] else {
                continue;
            };
            match self.group(&envelope, background, strongest, (candidate, level)) {
                Some(group) => {
                    let same = groups
                        .iter_mut()
                        .find(|other| (other.at - group.at).abs() <= AGREEMENT_SECONDS * self.rate);
                    match same {
                        Some(other) if other.level < group.level => *other = group,
                        Some(_) => {}
                        None => groups.push(group),
                    }
                }
                None if level >= significant
                    && unexplained.is_none_or(|(_, louder)| level > louder) =>
                {
                    unexplained = Some((candidate, level));
                }
                None => {}
            }
        }
        groups.retain(|group| group.level >= loudest * ratio(-AMBIGUITY_DB));
        let ticks_per_second = crate::host_time::ticks_per_second();
        let late_ms = |at: f64| (span.date(at) - sound_at[0]) / ticks_per_second * 1e3;
        debug!(
            loudest_db = %format!("{:.1}", db(loudest / background)),
            unexplained = unexplained.map(|(candidate, _)| candidate),
            groups = groups
                .iter()
                .map(|group| format!(
                    "{:+.2} ms {:.1} dB over the background, probes {:?}",
                    late_ms(group.at),
                    db(group.level / background),
                    group.probes
                ))
                .collect::<Vec<_>>()
                .join(", "),
            "calibration train groups"
        );
        if let Some((candidate, level)) = unexplained {
            let failure = self.failure(&envelope, background, &candidates[candidate]);
            debug!(
                candidate,
                under_loudest_db = %format!("{:.1}", db(loudest / level)),
                first_strongest_ms = %format!("{:+.2}", late_ms(candidates[candidate][0] as f64)),
                %failure,
                "calibration train refused: a candidate as loud as a train does not agree"
            );
            return Err(failure);
        }
        let taking_part: Vec<&Group> = groups
            .iter()
            .filter(|group| group.level >= significant)
            .collect();
        let arrival = taking_part
            .iter()
            .min_by(|a, b| a.at.total_cmp(&b.at))
            .expect("the loudest candidate forms a group");
        let ringing = RINGING_SECONDS * self.rate;
        let near_misses: Vec<&Group> = groups
            .iter()
            .filter(|group| group.at + ringing < arrival.at && group.level < significant)
            .collect();
        if !near_misses.is_empty() {
            let ambiguous = arrival
                .probes
                .iter()
                .filter(|k| near_misses.iter().any(|group| group.probes.contains(k)))
                .count();
            debug!(
                arrival_ms = %format!("{:+.2}", late_ms(arrival.at)),
                near_misses = near_misses
                    .iter()
                    .map(|group| format!("{:+.2} ms", late_ms(group.at)))
                    .collect::<Vec<_>>()
                    .join(", "),
                "calibration train ambiguous"
            );
            return Err(ProbeFailure::Ambiguous {
                ambiguous,
                measurable: arrival.probes.len() - ambiguous,
                required: AGREEING,
            });
        }
        self.measure_train(
            span,
            sound_at,
            &envelope,
            background,
            &taking_part,
            (arrival.candidate, candidates[arrival.candidate]),
        )
    }

    /// Claude: the median level of a candidate's probes heard, when at least
    /// `AGREEING` were.
    fn heard_level(
        &self,
        envelope: &[f64],
        background: f64,
        strongest: &[usize; PROBES],
    ) -> Option<f64> {
        let mut levels: Vec<f64> = strongest
            .iter()
            .map(|&peak| envelope[peak])
            .filter(|&level| level >= background * ratio(HEARD_DB))
            .collect();
        (levels.len() >= AGREEING).then(|| median(&mut levels))
    }

    /// Claude: the arrivals of a candidate's heard probes, each the earliest
    /// peak of its probe's cluster within `ARRIVAL_DB` of its strongest, as
    /// samples off the layout, with the probe's level and number, sorted by
    /// the first.
    fn cluster_arrivals(
        &self,
        envelope: &[f64],
        background: f64,
        strongest: &[usize; PROBES],
    ) -> Vec<(f64, f64, usize)> {
        let offset = |k: usize| (self.layout[k] * self.rate).round();
        let mut arrivals: Vec<(f64, f64, usize)> = strongest
            .iter()
            .enumerate()
            .filter(|&(_, &peak)| envelope[peak] >= background * ratio(HEARD_DB))
            .map(|(k, &peak)| {
                let arrival = self.cluster_arrival(envelope, peak);
                (arrival as f64 - offset(k), envelope[peak], k)
            })
            .collect();
        arrivals.sort_by(|a, b| a.0.total_cmp(&b.0));
        arrivals
    }

    /// Claude: the most of `arrivals` that lie within `AGREEMENT_SECONDS` of
    /// one another.
    fn agreeing<'a>(&self, arrivals: &'a [(f64, f64, usize)]) -> &'a [(f64, f64, usize)] {
        let agreement = AGREEMENT_SECONDS * self.rate;
        (0..arrivals.len())
            .map(|first| {
                let count = arrivals[first..]
                    .iter()
                    .take_while(|(at, _, _)| at - arrivals[first].0 <= agreement)
                    .count();
                &arrivals[first..first + count]
            })
            .max_by_key(|agreeing| agreeing.len())
            .unwrap_or(&[])
    }

    /// Claude: the group a candidate heard at `level` forms where at least
    /// `AGREEING` of its probes' arrivals agree.
    fn group(
        &self,
        envelope: &[f64],
        background: f64,
        strongest: &[usize; PROBES],
        (candidate, level): (usize, f64),
    ) -> Option<Group> {
        let arrivals = self.cluster_arrivals(envelope, background, strongest);
        let agreeing = self.agreeing(&arrivals);
        if agreeing.len() < AGREEING {
            return None;
        }
        let mut ats: Vec<f64> = agreeing.iter().map(|(at, _, _)| *at).collect();
        Some(Group {
            at: median(&mut ats),
            level,
            probes: agreeing.iter().map(|(_, _, k)| *k).collect(),
            candidate,
        })
    }

    /// Claude: how a candidate that forms no group fails to agree, from its
    /// own probes' arrivals: on a trending line (`Moved`), or otherwise
    /// scattered (`Disagree`).
    fn failure(
        &self,
        envelope: &[f64],
        background: f64,
        strongest: &[usize; PROBES],
    ) -> ProbeFailure {
        let arrivals = self.cluster_arrivals(envelope, background, strongest);
        let by_probe: Vec<(usize, f64)> = arrivals.iter().map(|&(at, _, k)| (k, at)).collect();
        self.moved(&by_probe).unwrap_or(ProbeFailure::Disagree {
            spread_ms: (arrivals[arrivals.len() - 1].0 - arrivals[0].0) / self.rate * 1e3,
        })
    }

    /// Claude: the earliest peak within `ARRIVAL_DB` of `strongest`, no more
    /// than `RINGING_SECONDS` before it, that no later peak's sidelobes
    /// explain: where a probe's own cluster of near-equal arrivals starts.
    fn cluster_arrival(&self, envelope: &[f64], strongest: usize) -> usize {
        let level = envelope[strongest];
        let from = strongest
            .saturating_sub((RINGING_SECONDS * self.rate) as usize)
            .max(1);
        (from..strongest)
            .find(|&i| {
                is_peak(envelope, i)
                    && envelope[i] >= level * ratio(-ARRIVAL_DB)
                    && !self.is_sidelobe(envelope, i)
            })
            .unwrap_or(strongest)
    }

    /// Claude: the train candidate number `candidate` makes of the probes'
    /// strongest peaks `strongest`, each probe arriving at the earliest peak
    /// the arrival rule allows, none where one of `groups` places another of
    /// its probes, measured where at least `AGREEING` agree.
    fn measure_train(
        &self,
        span: &Span,
        sound_at: [f64; 5],
        envelope: &[f64],
        background: f64,
        groups: &[&Group],
        (candidate, strongest): (usize, [usize; PROBES]),
    ) -> Result<Measured, ProbeFailure> {
        let ticks_per_second = crate::host_time::ticks_per_second();
        let detailed = tracing::enabled!(Level::DEBUG);
        let late_ms = |index: usize, k: usize| {
            (span.date(index as f64) - sound_at[k]) / ticks_per_second * 1e3
        };
        debug!(
            candidate,
            background_dbfs = %format!("{:.1}", db(background / self.floor) + FLOOR_DBFS),
            first_strongest_ms = %format!("{:+.2}", late_ms(strongest[0], 0)),
            "calibration train aligned"
        );
        let heard: Vec<(usize, usize)> = strongest
            .iter()
            .enumerate()
            .filter(|(_, peak)| envelope[**peak] >= background * ratio(HEARD_DB))
            .map(|(k, peak)| (k, *peak))
            .collect();
        for (k, &peak) in strongest.iter().enumerate() {
            debug!(
                probe = k,
                strongest_ms = %format!("{:+.2}", late_ms(peak, k)),
                above_background_db = %format!("{:.1}", db(envelope[peak] / background)),
                heard = heard.iter().any(|&(heard, _)| heard == k),
                "calibration probe"
            );
        }
        if heard.len() < AGREEING {
            let loudest_unheard = strongest
                .iter()
                .enumerate()
                .filter(|(k, _)| !heard.iter().any(|(heard, _)| heard == k))
                .map(|(_, &peak)| envelope[peak])
                .fold(0.0, f64::max);
            debug!(heard = heard.len(), "calibration train not found");
            return Err(ProbeFailure::NotFound {
                heard: heard.len(),
                required: AGREEING,
                above_background_db: db(loudest_unheard / background),
            });
        }

        let offset = |k: usize| (self.layout[k] * self.rate).round();
        let mut arrivals: Vec<(usize, f64)> = heard
            .iter()
            .filter_map(|&(k, peak)| {
                let others: Vec<f64> = groups
                    .iter()
                    .flat_map(|group| {
                        group
                            .probes
                            .iter()
                            .filter(move |&&j| j != k)
                            .map(move |&j| group.at + offset(j))
                    })
                    .collect();
                let found = self.arrival(envelope, peak, background, &others, detailed);
                let level = envelope[peak];
                let peaks = |indices: &[usize]| {
                    indices
                        .iter()
                        .map(|&i| {
                            format!(
                                "{:+.2} ms {:.1} dB under",
                                late_ms(i, k),
                                db(level / envelope[i])
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let separations = || {
                    found
                        .near_misses
                        .iter()
                        .map(|&i| format!("{:.2} ms", (found.within - i) as f64 / self.rate * 1e3))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                debug!(
                    probe = k,
                    arrival_ms = %format!("{:+.2}", late_ms(found.within, k)),
                    arrival_under_db = %format!("{:.1}", db(level / envelope[found.within])),
                    ambiguous = !found.near_misses.is_empty(),
                    near_misses = peaks(&found.near_misses),
                    near_misses_before_arrival = separations(),
                    sidelobes = peaks(&found.sidelobes),
                    "calibration probe arrival"
                );
                Some((k, found.arrival()? as f64 - offset(k)))
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
        let against_schedule = |arrivals: &[(usize, f64)]| {
            arrivals
                .iter()
                .map(|&(k, at)| {
                    let late = (span.date(at + offset(k)) - sound_at[k]) / ticks_per_second;
                    format!("probe {k} {:+.2} ms", late * 1e3)
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        debug!(
            arrivals = against_schedule(&arrivals),
            agreeing = against_schedule(agreeing),
            "calibration train agreement"
        );
        if agreeing.len() < AGREEING {
            return Err(if arrivals.len() < AGREEING {
                ProbeFailure::Ambiguous {
                    ambiguous: heard.len() - arrivals.len(),
                    measurable: arrivals.len(),
                    required: AGREEING,
                }
            } else {
                self.moved(&arrivals).unwrap_or(ProbeFailure::Disagree {
                    spread_ms: (arrivals[arrivals.len() - 1].1 - arrivals[0].1) / self.rate * 1e3,
                })
            });
        }

        let mut probes: Vec<(f64, f64)> = agreeing
            .iter()
            .map(|&(k, at)| {
                let index = refine(envelope, at + offset(k));
                (
                    (span.date(index) - sound_at[k]) / ticks_per_second,
                    sound_at[k],
                )
            })
            .collect();
        let (drift, _, _) = fit(&probes
            .iter()
            .map(|&(late, at)| (at / ticks_per_second, late))
            .collect::<Vec<_>>());
        probes.sort_by(|a, b| a.0.total_cmp(&b.0));
        let n = probes.len() as f64;
        let measured = Measured {
            residual: probes.iter().map(|(late, _)| late).sum::<f64>() / n,
            spread: probes[probes.len() - 1].0 - probes[0].0,
            at: probes.iter().map(|(_, at)| at).sum::<f64>() / n,
            drift,
        };
        debug!(
            residual_ms = %format!("{:+.3}", measured.residual * 1e3),
            spread_ms = %format!("{:.3}", measured.spread * 1e3),
            drift_ms_per_second = %format!("{:+.4}", measured.drift * 1e3),
            "calibration train measured"
        );
        Ok(measured)
    }

    /// Claude: how the arrivals moved, `(k, samples off the layout)` sorted by
    /// the latter, when there are enough to make a match, they lie within
    /// `AGREEMENT_SECONDS` of a line through their probes' times, and that
    /// line's slope stands `TREND` standard errors from zero, as the arrivals
    /// of a destination whose timing moves steadily do; none when they
    /// scatter, as probes heard by different paths do.
    fn moved(&self, arrivals: &[(usize, f64)]) -> Option<ProbeFailure> {
        if arrivals.len() < AGREEING {
            return None;
        }
        let points: Vec<(f64, f64)> = arrivals
            .iter()
            .map(|&(k, at)| (self.layout[k], at / self.rate))
            .collect();
        let (slope, mean_t, mean_at) = fit(&points);
        let residuals: Vec<f64> = points
            .iter()
            .map(|(t, at)| at - (mean_at + slope * (t - mean_t)))
            .collect();
        let on_a_line = residuals
            .iter()
            .all(|residual| residual.abs() <= AGREEMENT_SECONDS);
        let scatter = residuals.iter().map(|r| r * r).sum::<f64>() / (points.len() - 2) as f64;
        let spread_of_times: f64 = points.iter().map(|(t, _)| (t - mean_t).powi(2)).sum();
        let standard_error = (scatter / spread_of_times).sqrt();
        let trending = slope.abs() > TREND * standard_error;
        let spread = arrivals[arrivals.len() - 1].1 - arrivals[0].1;
        (on_a_line && trending).then(|| ProbeFailure::Moved {
            spread_ms: spread / self.rate * 1e3,
            ms_per_second: slope * 1e3,
        })
    }

    /// Claude: candidate trains, each its probes' strongest peaks near their
    /// exact places in an alignment of the layout ranked best among the
    /// alignments within two blocks of it, so that an
    /// alignment catching only part of a train's peaks is no candidate: the
    /// most probes heard first, and of those the loudest in decibels summed,
    /// so that one loud sound cannot outweigh a whole train.
    fn candidates(&self, envelope: &[f64], background: f64) -> Vec<[usize; PROBES]> {
        let block = ((NEAR_SECONDS * self.rate) as usize).max(1);
        let offsets = self
            .layout
            .map(|offset| (offset * self.rate).round() as usize);
        let phases = offsets.map(|offset| offset % block);
        let places = offsets.map(|offset| offset / block);
        let blocks = phases.map(|phase| {
            envelope[phase.min(envelope.len())..]
                .chunks(block)
                .map(|chunk| chunk.iter().copied().fold(0.0, f64::max))
                .collect::<Vec<f64>>()
        });
        let near = |k: usize, alignment: usize| {
            let at = alignment + places[k];
            at.saturating_sub(1)..(at + 2).min(blocks[k].len())
        };
        let heard = background * ratio(HEARD_DB);
        let score = |alignment: usize| -> (usize, f64) {
            (0..PROBES)
                .map(|k| near(k, alignment).map(|b| blocks[k][b]).fold(0.0, f64::max))
                .fold((0, 0.0), |(count, sum), level| {
                    (
                        count + usize::from(level >= heard),
                        sum + db(level.max(background) / background),
                    )
                })
        };
        let span = (0..PROBES)
            .map(|k| blocks[k].len().saturating_sub(places[k]))
            .min()
            .unwrap_or(0);
        let scores: Vec<(usize, f64)> = (0..span.max(1)).map(score).collect();
        let better = |a: usize, b: usize| {
            let ((count_a, sum_a), (count_b, sum_b)) = (scores[a], scores[b]);
            count_a.cmp(&count_b).then(sum_a.total_cmp(&sum_b))
        };
        let mut ranked: Vec<usize> = (0..scores.len())
            .filter(|&alignment| {
                (alignment.saturating_sub(2)..(alignment + 3).min(scores.len())).all(|other| {
                    better(alignment, other).is_ge()
                        && (other >= alignment || better(alignment, other).is_gt())
                })
            })
            .collect();
        ranked.sort_by(|&a, &b| better(b, a));
        let mut candidates: Vec<[usize; PROBES]> = Vec::new();
        for alignment in ranked {
            let strongest = std::array::from_fn(|k| {
                let around = near(k, alignment);
                let from = (phases[k] + around.start * block).min(envelope.len() - 1);
                let to = (phases[k] + around.end * block).min(envelope.len());
                (from..to.max(from + 1).min(envelope.len()))
                    .max_by(|&a, &b| envelope[a].total_cmp(&envelope[b]))
                    .unwrap_or(from)
            });
            if !candidates.contains(&strongest) {
                candidates.push(strongest);
            }
        }
        candidates
    }

    /// Claude: the earliest peak within `ARRIVAL_DB` of `strongest`, at most
    /// `LOOKBACK_SECONDS` before it, that no later peak's sidelobes explain
    /// and that does not lie in the cluster of another probe arriving at one
    /// of `others`; none when a peak in the band down to `AMBIGUITY_DB` comes
    /// more than `RINGING_SECONDS` before that. `detailed` keeps the peaks
    /// excluded as sidelobes.
    fn arrival(
        &self,
        envelope: &[f64],
        strongest: usize,
        background: f64,
        others: &[f64],
        detailed: bool,
    ) -> Found {
        let level = envelope[strongest];
        let from = strongest
            .saturating_sub((LOOKBACK_SECONDS * self.rate) as usize)
            .max(1);
        let (agreement, ringing) = (AGREEMENT_SECONDS * self.rate, RINGING_SECONDS * self.rate);
        let another_probe = |i: usize| {
            others
                .iter()
                .any(|&at| (at - agreement..=at + ringing).contains(&(i as f64)))
        };
        let mut sidelobes = Vec::new();
        let candidates: Vec<usize> = (from..strongest)
            .filter(|&i| {
                let considered = is_peak(envelope, i)
                    && !another_probe(i)
                    && envelope[i] >= level * ratio(-AMBIGUITY_DB)
                    && envelope[i] >= background * ratio(HEARD_DB);
                let explained = considered && self.is_sidelobe(envelope, i);
                if explained && detailed {
                    sidelobes.push(i);
                }
                considered && !explained
            })
            .collect();
        let arrival = candidates
            .iter()
            .copied()
            .find(|&i| envelope[i] >= level * ratio(-ARRIVAL_DB))
            .unwrap_or(strongest);
        let ringing = (RINGING_SECONDS * self.rate) as usize;
        let near_misses: Vec<usize> = candidates
            .iter()
            .copied()
            .take_while(|&i| i + ringing < arrival)
            .collect();
        Found {
            within: arrival,
            near_misses,
            sidelobes,
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

/// Claude: the least-squares line through `points`: its slope, and the
/// means it passes through.
fn fit(points: &[(f64, f64)]) -> (f64, f64, f64) {
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
    (sxy / sxx, mean_x, mean_y)
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

    /// Claude: the role a test's trains play as, unless it says otherwise.
    const ROLE: Role = Role::Reference;
    const ROLES: [Role; 2] = [Role::Reference, Role::Output];

    fn train() -> [f64; PROBES] {
        layout(ROLE)
    }

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

    /// Claude: a 5.3 s recording at `rate` of trains scheduled to sound
    /// `SOUND` s in, each `(role, paths)` played with `role`'s probe on its
    /// layout and arriving along each of `paths`, over noise of `noise` RMS,
    /// and when each probe of `scheduled`'s train was scheduled.
    fn recorded_as(
        scheduled: Role,
        rate: f64,
        trains: &[(Role, &[Path])],
        noise: f64,
    ) -> (Span, [f64; 5]) {
        let ticks = host_time::ticks_per_second();
        let origin = 1e12;
        let mut deviates = Noise(0x9e37_79b9_7f4a_7c15 ^ rate.to_bits());
        let samples = (0..(5.3 * rate) as usize)
            .map(|i| {
                let t = i as f64 / rate;
                let train: f64 = trains
                    .iter()
                    .flat_map(|&(role, paths)| paths.iter().map(move |path| (role, path)))
                    .flat_map(|(role, path)| {
                        layout(role)
                            .into_iter()
                            .enumerate()
                            .map(move |(k, offset)| {
                                let heard = path.probes.as_ref().is_none_or(|on| on.contains(&k));
                                if heard {
                                    path.gain * probe_at(role, t - SOUND - offset - path.delay)
                                } else {
                                    0.0
                                }
                            })
                    })
                    .sum();
                (train + noise * deviates.normal()) as f32
            })
            .collect();
        let sound_at = layout(scheduled).map(|offset| origin + (SOUND + offset) * ticks);
        (Span::linear(samples, origin, ticks / rate), sound_at)
    }

    fn recorded(rate: f64, paths: &[Path], noise: f64) -> (Span, [f64; 5]) {
        recorded_as(ROLE, rate, &[(ROLE, paths)], noise)
    }

    fn measure_as(
        role: Role,
        rate: f64,
        taps: &[(f64, f64)],
        noise: f64,
    ) -> Result<Measured, ProbeFailure> {
        let (span, sound_at) = recorded_as(role, rate, &[(role, &paths(taps))], noise);
        Detector::new(rate, role).measure(&span, sound_at)
    }

    fn measure(rate: f64, taps: &[(f64, f64)], noise: f64) -> Result<Measured, ProbeFailure> {
        measure_as(ROLE, rate, taps, noise)
    }

    fn db(db: f64) -> f64 {
        10f64.powf(db / 20.0)
    }

    #[test]
    fn a_train_is_found_where_it_arrived_to_a_fraction_of_a_sample() {
        for role in ROLES {
            for rate in [16_000.0, 44_100.0, 48_000.0, 96_000.0] {
                for frames in [0.0, 0.25, 0.5, 0.73, 4801.4] {
                    let delay = 0.0123 + frames / rate;
                    let measured = measure_as(role, rate, &[(delay, 0.3)], 1e-4).unwrap();
                    let error = (measured.residual - delay) * rate;
                    assert!(
                        error.abs() < 0.05,
                        "{role}, {rate} Hz, {delay} s: {error} frames off"
                    );
                    assert!(
                        measured.spread * rate < 0.1,
                        "{role}, {rate} Hz: {measured:?}"
                    );
                }
            }
        }
    }

    /// Claude: how far above the background, in dB, a path of `gain` peaks
    /// over noise of `noise` RMS at 48 kHz: the probe's energy against the
    /// median envelope of filtered noise, 1.177 standard deviations of it.
    fn level_db(gain: f64, noise: f64) -> f64 {
        let template: Vec<f32> = probe(48_000.0, ROLE);
        let energy: f64 = template.iter().map(|s| f64::from(*s).powi(2)).sum();
        let peak = gain * energy;
        20.0 * (peak / (1.1774 * noise * energy.sqrt())).log10()
    }

    #[test]
    fn a_single_path_shows_no_early_arrival_and_no_ambiguity() {
        for (role, (gain, noise)) in ROLES.into_iter().flat_map(|role| {
            [
                (1.0, 1e-6),
                (0.5, 1e-4),
                (0.05, 1e-3),
                (0.01, 3e-3),
                (0.3, 0.02),
            ]
            .map(|level| (role, level))
        }) {
            let delay = 0.0371;
            let label = format!("{role}, gain {gain}, noise {noise}");
            let measured = measure_as(role, 48_000.0, &[(delay, gain)], noise)
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
                heard: 0,
                required: 4,
                above_background_db,
            }) => assert!(above_background_db < 20.0, "{above_background_db}"),
            other => panic!("{other:?}"),
        }
    }

    /// Claude: a quieter train, its probes about 35 dB over the background,
    /// and one loud noise burst after it, as a knock or a word: the train is
    /// still found where it arrived.
    #[test]
    fn one_loud_disturbance_does_not_displace_a_quieter_train() {
        let (gain, noise) = (0.01, 1e-3);
        assert!(
            (30.0..40.0).contains(&level_db(gain, noise)),
            "{}",
            level_db(gain, noise)
        );
        let (mut span, sound_at) = recorded(48_000.0, &paths(&[(0.031, gain)]), noise);
        let mut deviates = Noise(7);
        let burst = (3.9 * 48_000.0) as usize..(4.0 * 48_000.0) as usize;
        for sample in &mut span.samples[burst] {
            *sample += (0.3 * deviates.normal()) as f32;
        }
        let measured = Detector::new(48_000.0, ROLE)
            .measure(&span, sound_at)
            .unwrap_or_else(|failure| panic!("{failure}"));
        assert!((measured.residual - 0.031).abs() < 1e-5, "{measured:?}");
    }

    /// Claude: four strong probes agreeing at +31 ms, and all five 40 dB
    /// weaker, scattered about +800 ms, as a second, quieter sound of the same
    /// probes might be: the agreeing train is measured, though the scattered
    /// one has more probes heard.
    #[test]
    fn a_valid_train_beats_one_with_more_probes_heard_that_disagree() {
        let mut paths = vec![Path {
            delay: 0.031,
            gain: 0.3,
            probes: Some(vec![0, 1, 2, 3]),
        }];
        for (k, scatter) in [-0.002, 0.0015, -0.001, 0.002, 0.0].into_iter().enumerate() {
            paths.push(Path {
                delay: 0.8 + scatter,
                gain: 0.3 * db(-40.0),
                probes: Some(vec![k]),
            });
        }
        let (span, sound_at) = recorded(48_000.0, &paths, 1e-4);
        let measured = Detector::new(48_000.0, ROLE)
            .measure(&span, sound_at)
            .unwrap_or_else(|failure| panic!("{failure}"));
        assert!((measured.residual - 0.031).abs() < 1e-5, "{measured:?}");
    }

    /// Claude: a train heard 40 dB under five louder probes that scatter
    /// about +800 ms is not measured while they stand unexplained: their
    /// failure is the train's.
    #[test]
    fn a_quieter_valid_train_under_a_louder_scattered_set_is_not_measured() {
        let mut paths = vec![Path {
            delay: 0.031,
            gain: 0.3 * db(-40.0),
            probes: Some(vec![0, 1, 2, 3]),
        }];
        for (k, scatter) in [-0.002, 0.0015, -0.001, 0.002, 0.0].into_iter().enumerate() {
            paths.push(Path {
                delay: 0.8 + scatter,
                gain: 0.3,
                probes: Some(vec![k]),
            });
        }
        let (span, sound_at) = recorded(48_000.0, &paths, 1e-4);
        let failure = Detector::new(48_000.0, ROLE)
            .measure(&span, sound_at)
            .unwrap_err();
        let ProbeFailure::Disagree { spread_ms } = failure else {
            panic!("{failure:?}")
        };
        assert!((spread_ms - 4.0).abs() < 0.05, "{spread_ms}");
    }

    /// Claude: an output whose probes arrive 2.3 ms a second later each, and
    /// a train 40 or 14 dB under it that stands still, as the reference played
    /// before it might leak into its window: the train moved, wherever the
    /// output has moved to, and the quieter train is not measured instead.
    #[test]
    fn a_quieter_still_train_does_not_hide_a_louder_one_that_moved() {
        for (from, under_db) in [(0.8, 40.0), (0.824_38, 40.0), (0.8, 14.0), (0.824_38, 14.0)] {
            let mut paths: Vec<Path> = train()
                .iter()
                .enumerate()
                .map(|(k, offset)| Path {
                    delay: from + 0.0023 * offset,
                    gain: 0.3,
                    probes: Some(vec![k]),
                })
                .collect();
            paths.push(Path {
                delay: 0.031,
                gain: 0.3 * db(-under_db),
                probes: None,
            });
            let label = format!("from {from} s, {under_db} dB under");
            let (span, sound_at) = recorded(48_000.0, &paths, 1e-4);
            let failure = Detector::new(48_000.0, ROLE)
                .measure(&span, sound_at)
                .map(|measured| panic!("{label}: {measured:?}"))
                .unwrap_err();
            let ProbeFailure::Moved { ms_per_second, .. } = failure else {
                panic!("{label}: {failure:?}")
            };
            assert!(
                (ms_per_second - 2.3).abs() < 0.02,
                "{label}: {ms_per_second}"
            );
        }
    }

    /// Claude: a direct path 10 dB under an echo 400 ms later is the arrival,
    /// but with five louder probes scattered about +1900 ms, unexplained, the
    /// train is not measured at all: neither at the direct path nor at the
    /// echo.
    #[test]
    fn a_louder_sound_left_unexplained_leaves_no_path_measured() {
        let mut paths = paths(&[(0.012, 0.3 * db(-22.0)), (0.412, 0.3 * db(-12.0))]);
        let (span, sound_at) = recorded(48_000.0, &paths, 1e-4);
        let measured = Detector::new(48_000.0, ROLE)
            .measure(&span, sound_at)
            .unwrap_or_else(|failure| panic!("{failure}"));
        assert!((measured.residual - 0.012).abs() < 1e-5, "{measured:?}");
        for (k, scatter) in [-0.002, 0.0015, -0.001, 0.002, 0.0].into_iter().enumerate() {
            paths.push(Path {
                delay: 1.9 + scatter,
                gain: 0.3,
                probes: Some(vec![k]),
            });
        }
        let (span, sound_at) = recorded(48_000.0, &paths, 1e-4);
        let failure = Detector::new(48_000.0, ROLE)
            .measure(&span, sound_at)
            .map(|measured| panic!("{measured:?}"))
            .unwrap_err();
        let ProbeFailure::Disagree { spread_ms } = failure else {
            panic!("{failure:?}")
        };
        assert!((spread_ms - 4.0).abs() < 0.05, "{spread_ms}");
    }

    /// Claude: four strong probes agreeing at +31 ms, and `weak` paths 40 dB
    /// under them, each `(delay, probe)`.
    fn strong_train_with(weak: &[(f64, usize)]) -> Result<Measured, ProbeFailure> {
        let mut paths = vec![Path {
            delay: 0.031,
            gain: 0.3,
            probes: Some(vec![0, 1, 2, 3]),
        }];
        paths.extend(weak.iter().map(|&(delay, probe)| Path {
            delay,
            gain: 0.3 * db(-40.0),
            probes: Some(vec![probe]),
        }));
        let (span, sound_at) = recorded(48_000.0, &paths, 1e-4);
        Detector::new(48_000.0, ROLE).measure(&span, sound_at)
    }

    /// Claude: a weaker set of probes scattered 231 ms earlier, failing to
    /// agree, does not hide the train after it.
    #[test]
    fn a_weaker_scattered_set_before_a_valid_train_does_not_hide_it() {
        let scattered: Vec<(f64, usize)> = [-0.002, 0.0015, -0.001, 0.002, 0.0]
            .into_iter()
            .enumerate()
            .map(|(k, scatter)| (-0.2 + scatter, k))
            .collect();
        let measured = strong_train_with(&scattered).unwrap_or_else(|failure| panic!("{failure}"));
        assert!((measured.residual - 0.031).abs() < 1e-5, "{measured:?}");
    }

    /// Claude: a copy of the train 40 dB weaker and 400 ms later, its probes
    /// agreeing, is not the arrival: the train is.
    #[test]
    fn a_weaker_agreeing_copy_after_a_valid_train_is_not_its_arrival() {
        let copy: Vec<(f64, usize)> = (0..PROBES).map(|k| (0.431, k)).collect();
        let measured = strong_train_with(&copy).unwrap_or_else(|failure| panic!("{failure}"));
        assert!((measured.residual - 0.031).abs() < 1e-5, "{measured:?}");
    }

    /// Claude: one probe heard loudly and the others not at all says so,
    /// rather than that the loudest fell short.
    #[test]
    fn a_train_with_one_probe_heard_is_not_found_and_says_how_many_were() {
        let paths = [
            Path {
                delay: 0.02,
                gain: 0.3,
                probes: Some(vec![2]),
            },
            Path {
                delay: 0.02,
                gain: 0.0002,
                probes: Some(vec![0, 1, 3, 4]),
            },
        ];
        let (span, sound_at) = recorded(48_000.0, &paths, 0.01);
        let failure = Detector::new(48_000.0, ROLE)
            .measure(&span, sound_at)
            .unwrap_err();
        let ProbeFailure::NotFound {
            heard: 1,
            required: 4,
            above_background_db,
        } = failure
        else {
            panic!("{failure:?}")
        };
        assert!(above_background_db < 20.0, "{above_background_db}");
        let said = failure.to_string();
        assert!(
            said.contains("1 of the 5 stood 20 dB above the background, where 4 must"),
            "{said}"
        );
    }

    /// Claude: probes heard more than once on all but three, so too few to
    /// agree can be measured, are ambiguous, and say how many were.
    #[test]
    fn too_few_measurable_probes_left_by_ambiguity_are_ambiguous_not_moved() {
        let mut taps = paths(&[(0.032, 0.3)]);
        taps.push(Path {
            delay: 0.012,
            gain: 0.3 * db(-18.0),
            probes: Some(vec![0, 1]),
        });
        let (span, sound_at) = recorded(48_000.0, &taps, 1e-5);
        let failure = Detector::new(48_000.0, ROLE)
            .measure(&span, sound_at)
            .unwrap_err();
        assert_eq!(
            failure,
            ProbeFailure::Ambiguous {
                ambiguous: 2,
                measurable: 3,
                required: 4,
            }
        );
        let said = failure.to_string();
        assert!(
            said.contains("leaving 3 measurable where 4 must agree"),
            "{said}"
        );
    }

    /// Claude: arrivals that lie within 1 ms of a line through their probes'
    /// times but scatter about it with no trend, three of them agreeing, did
    /// not move: they disagree.
    #[test]
    fn arrivals_near_a_flat_line_without_a_trend_disagree_rather_than_moved() {
        let paths: Vec<Path> = [-0.000_652, 0.000_569, -0.000_001, 0.000_625, -0.000_542]
            .into_iter()
            .enumerate()
            .map(|(k, scatter)| Path {
                delay: 0.02 + scatter,
                gain: 0.3,
                probes: Some(vec![k]),
            })
            .collect();
        let (span, sound_at) = recorded(48_000.0, &paths, 1e-5);
        let failure = Detector::new(48_000.0, ROLE)
            .measure(&span, sound_at)
            .unwrap_err();
        let ProbeFailure::Disagree { spread_ms } = failure else {
            panic!("{failure:?}")
        };
        assert!((spread_ms - 1.277).abs() < 0.02, "{spread_ms}");
    }

    /// Claude: a train played by a receiver that slews its clock 2.3 ms a
    /// second, each probe arriving later than the one before, moved: it is
    /// not ambiguous.
    #[test]
    fn a_train_whose_probes_arrive_ever_later_moved() {
        let paths: Vec<Path> = train()
            .iter()
            .enumerate()
            .map(|(k, offset)| Path {
                delay: 0.02 + 0.0023 * offset,
                gain: 0.3,
                probes: Some(vec![k]),
            })
            .collect();
        let (span, sound_at) = recorded(48_000.0, &paths, 1e-4);
        let failure = Detector::new(48_000.0, ROLE)
            .measure(&span, sound_at)
            .unwrap_err();
        let ProbeFailure::Moved {
            spread_ms,
            ms_per_second,
        } = failure
        else {
            panic!("{failure:?}")
        };
        assert!((spread_ms - 5.98).abs() < 0.05, "{spread_ms}");
        assert!((ms_per_second - 2.3).abs() < 0.02, "{ms_per_second}");
        let said = failure.to_string();
        assert!(
            said.contains("timing moved") && said.contains("drifted +2.3 ms a second"),
            "{said}"
        );
    }

    /// Claude: four probes heard, each arriving about 2 ms a second later
    /// than the one before, scattered about that line by up to 0.13 ms, and
    /// the fifth not heard: the train moved.
    #[test]
    fn four_probes_arriving_ever_later_through_noise_moved() {
        let paths: Vec<Path> = [0.000_08, -0.000_11, 0.000_12, -0.000_09]
            .into_iter()
            .enumerate()
            .map(|(k, scatter)| Path {
                delay: 0.02 + 0.002 * train()[k] + scatter,
                gain: 0.3,
                probes: Some(vec![k]),
            })
            .collect();
        let (span, sound_at) = recorded(48_000.0, &paths, 1e-4);
        let failure = Detector::new(48_000.0, ROLE)
            .measure(&span, sound_at)
            .unwrap_err();
        let ProbeFailure::Moved {
            spread_ms,
            ms_per_second,
        } = failure
        else {
            panic!("{failure:?}")
        };
        assert!((spread_ms - 3.43).abs() < 0.05, "{spread_ms}");
        assert!((ms_per_second - 1.96).abs() < 0.05, "{ms_per_second}");
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

    /// Claude: four probes heard at +12 ms and, 3.5 dB louder, at +14 ms, and
    /// a copy of the train at +512 ms on the other four, so that where the
    /// copy's first probe would be, had it been heard, the second probe
    /// arrives. 40 dB under, the copy removes no arrival, and the train
    /// arrives at +12 ms. 20 dB under, too faint to take part, it explains
    /// none of its own probes, two of which come 0.1 and 0.2 s before the
    /// train's third and fourth in the band where an earlier path makes a
    /// probe ambiguous: the train is.
    #[test]
    fn a_copy_of_the_train_removes_no_arrival_and_one_too_faint_to_take_part_is_ambiguous() {
        for copy_gain in [0.003, 0.03] {
            let on = |delay: f64, gain: f64, probes: &[usize]| Path {
                delay,
                gain,
                probes: Some(probes.to_vec()),
            };
            let paths = [
                on(0.012, 0.2, &[0, 1, 2, 3]),
                on(0.014, 0.3, &[0, 1, 2, 3]),
                on(0.512, copy_gain, &[1, 2, 3, 4]),
            ];
            let (span, sound_at) = recorded(48_000.0, &paths, 1e-4);
            match (
                Detector::new(48_000.0, ROLE).measure(&span, sound_at),
                copy_gain,
            ) {
                (Ok(measured), 0.003) => {
                    assert!((measured.residual - 0.012).abs() < 1e-5, "{measured:?}")
                }
                (
                    Err(ProbeFailure::Ambiguous {
                        ambiguous: 2,
                        measurable: 2,
                        required: 4,
                    }),
                    0.03,
                ) => {}
                (result, _) => panic!("copy at {copy_gain}: {result:?}"),
            }
        }
    }

    /// Claude: four probes heard at +30 ms, the second also 10 dB under at
    /// +12 ms, and a copy of the train 24 dB under them at +512 ms, where the
    /// first probe's place catches that earlier path: the copy, too faint to
    /// count, removes no arrival louder than itself, so the probes disagree.
    #[test]
    fn a_copy_too_faint_to_count_removes_no_louder_arrival() {
        let on = |delay: f64, gain: f64, probes: &[usize]| Path {
            delay,
            gain,
            probes: Some(probes.to_vec()),
        };
        let paths = [
            on(0.030, 0.3, &[0, 1, 2, 3]),
            on(0.012, 0.3 * db(-10.0), &[1]),
            on(0.512, 0.3 * db(-24.0), &[1, 2, 3, 4]),
        ];
        let (span, sound_at) = recorded(48_000.0, &paths, 1e-4);
        let failure = Detector::new(48_000.0, ROLE)
            .measure(&span, sound_at)
            .unwrap_err();
        let ProbeFailure::Disagree { spread_ms } = failure else {
            panic!("{failure:?}")
        };
        assert!((spread_ms - 18.0).abs() < 0.05, "{spread_ms}");
    }

    /// Claude: a direct path just inside the arrival bound under a later
    /// echo is the arrival, one in the band below it is ambiguous, and one
    /// further under leaves the echo the arrival, however far apart they lie.
    #[test]
    fn a_direct_path_under_an_echo_is_found_ambiguous_or_lost_by_how_far_under() {
        for (role, after) in ROLES
            .into_iter()
            .flat_map(|role| [0.02, 0.1, 0.4].map(|after| (role, after)))
        {
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
                let label = format!("{role}: echo {after} s later, direct {under_db} dB under");
                match (measure_as(role, 48_000.0, &taps, 1e-5), expected) {
                    (Ok(measured), Some(late)) => assert!(
                        (measured.residual - 0.012 - late).abs() < 2e-5,
                        "{label}: {measured:?}"
                    ),
                    (
                        Err(ProbeFailure::Ambiguous {
                            ambiguous: 5,
                            measurable: 0,
                            required: 4,
                        }),
                        None,
                    ) => {}
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

    /// Claude: an earlier path heard on some probes only gives arrivals that
    /// scatter rather than drift: the probes disagree, and their timing did
    /// not move.
    #[test]
    fn a_near_equal_earlier_arrival_on_some_probes_only_disagrees() {
        let mut taps = paths(&[(0.012, 0.3)]);
        taps.push(Path {
            delay: 0.002,
            gain: 0.29,
            probes: Some(vec![0, 2]),
        });
        let (span, sound_at) = recorded(48_000.0, &taps, 1e-4);
        let failure = Detector::new(48_000.0, ROLE)
            .measure(&span, sound_at)
            .unwrap_err();
        let ProbeFailure::Disagree { spread_ms } = failure else {
            panic!("{failure:?}")
        };
        assert!((spread_ms - 10.0).abs() < 0.05, "{spread_ms}");
        assert!(failure.to_string().contains("not as one train"));
        taps[1].probes = Some(vec![3]);
        let (span, sound_at) = recorded(48_000.0, &taps, 1e-4);
        let measured = Detector::new(48_000.0, ROLE)
            .measure(&span, sound_at)
            .unwrap();
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

    const SOUNDBAR_FIRST: f64 = -0.040_08;

    /// Claude: a soundbar as measured: drivers arriving near-equal over 2 ms
    /// from `SOUNDBAR_FIRST`, the strongest of them changing from probe to
    /// probe, reflected 14 and 20 ms after its strongest, and sounding 19 dB
    /// under that 2.6 ms ahead of its first arrival: 4.6 ms ahead of the
    /// strongest on some probes.
    fn soundbar() -> Vec<Path> {
        let (first, strongest) = (SOUNDBAR_FIRST, 0.25 * db(1.0));
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
        soundbar
    }

    /// Claude: speakers built as measured ones sound, each found at its
    /// first arrival on every probe without ambiguity: built-in speakers that
    /// ring ahead of a sharp arrival, 27 to 32 dB under it within 3 ms, and
    /// the soundbar.
    #[test]
    fn speakers_shaped_as_measured_are_found_at_their_first_arrival() {
        let built_in = 0.000_35;
        let speakers = paths(&[
            (built_in, 0.3),
            (built_in - 0.001_75, 0.3 * db(-32.0)),
            (built_in - 0.002_77, 0.3 * db(-31.0)),
        ]);
        for role in ROLES {
            for (label, paths, arrival) in [
                ("built-in", speakers.clone(), built_in),
                ("soundbar", soundbar(), SOUNDBAR_FIRST),
            ] {
                let (span, sound_at) = recorded_as(role, 48_000.0, &[(role, &paths)], 1e-5);
                let measured = Detector::new(48_000.0, role)
                    .measure(&span, sound_at)
                    .unwrap_or_else(|e| panic!("{role}, {label}: {e}"));
                assert!(
                    (measured.residual - arrival).abs() < 5e-5 && measured.spread < 1e-4,
                    "{role}, {label}: {measured:?}"
                );
            }
        }
    }

    /// Claude: the soundbar, its strongest driver changing from probe to
    /// probe, and a single path of the train 400 ms later and 6 dB under it:
    /// the soundbar's first arrival is the arrival.
    #[test]
    fn a_train_whose_strongest_driver_changes_is_the_arrival_before_a_later_copy() {
        let mut taps = soundbar();
        let strongest = taps.iter().map(|path| path.gain).fold(0.0, f64::max);
        taps.extend(paths(&[(SOUNDBAR_FIRST + 0.4, strongest * db(-6.0))]));
        let (span, sound_at) = recorded(48_000.0, &taps, 1e-5);
        let measured = Detector::new(48_000.0, ROLE)
            .measure(&span, sound_at)
            .unwrap_or_else(|failure| panic!("{failure}"));
        assert!(
            (measured.residual - SOUNDBAR_FIRST).abs() < 5e-5,
            "{measured:?}"
        );
    }

    /// Claude: each role's filter answers the other role's probe 32.6 dB
    /// under its own, smeared over a third of a second rather than a peak,
    /// at every rate a recording may run at.
    #[test]
    fn each_filter_answers_the_other_roles_probe_32_db_under_its_own() {
        for rate in [16_000.0, 44_100.0, 48_000.0, 96_000.0] {
            for (role, other) in [
                (Role::Reference, Role::Output),
                (Role::Output, Role::Reference),
            ] {
                let mut detector = Detector::new(rate, role);
                let silence = vec![0.0f32; (0.3 * rate) as usize];
                let surrounded = |probe: Vec<f32>| [&silence[..], &probe, &silence].concat();
                let peak = |envelope: Vec<f64>| envelope.into_iter().fold(0.0, f64::max);
                let own = peak(detector.envelope(&surrounded(probe(rate, role))));
                let other = peak(detector.envelope(&surrounded(probe(rate, other))));
                let under = super::db(own / other);
                assert!(
                    (32.5..33.0).contains(&under),
                    "{role}, {rate} Hz: {under} dB"
                );
            }
        }
    }

    /// Claude: the output's probe, the reference's reversed in time, has the
    /// same correlation with itself, so the same sidelobes.
    #[test]
    fn both_probes_have_the_same_sidelobes() {
        for rate in [16_000.0, 48_000.0, 96_000.0] {
            let [reference, output] = ROLES.map(|role| Detector::new(rate, role).sidelobes);
            assert_eq!(reference.len(), output.len());
            assert!(
                reference
                    .iter()
                    .zip(&output)
                    .all(|(a, b)| (a - b).abs() < 1e-9)
            );
        }
    }

    /// Claude: the reference's train leaking into the output's recording as
    /// loud as the output, 51.5 ms ahead of it, is not measured as the
    /// output: the output is, and with the output silent, nothing is.
    #[test]
    fn a_reference_train_leaking_into_an_output_recording_is_not_measured_as_the_output() {
        let output = paths(&[(0.0817, 0.3)]);
        let leak = paths(&[(0.0302, 0.3)]);
        let (span, sound_at) = recorded_as(
            Role::Output,
            48_000.0,
            &[(Role::Output, &output), (Role::Reference, &leak)],
            1e-4,
        );
        let measured = Detector::new(48_000.0, Role::Output)
            .measure(&span, sound_at)
            .unwrap_or_else(|failure| panic!("{failure}"));
        assert!((measured.residual - 0.0817).abs() < 1e-5, "{measured:?}");
        let (span, sound_at) =
            recorded_as(Role::Output, 48_000.0, &[(Role::Reference, &leak)], 1e-4);
        let result = Detector::new(48_000.0, Role::Output).measure(&span, sound_at);
        assert!(result.is_err(), "{result:?}");
    }
}
