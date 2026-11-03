use super::{ClockIdentity, PtpTime, Timebase, Timeline, signed_nanos};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub(super) const SILENCE: Duration = Duration::from_secs(1);
pub(super) const HOLDOVER: Duration = Duration::from_secs(15);
const WINDOW: usize = 128;
const MIN_DELAY_MARGIN_NS: f64 = 200_000.0;
const MAX_DELAY_MARGIN_NS: f64 = 4_000_000.0;
const MAX_OFFSET_UNCERTAINTY_NS: f64 = 5_000_000.0;
const RATE_PRIOR_UNCERTAINTY: f64 = 100e-6;
const PHASE_STEP_NS: i128 = 5_000_000;

#[derive(Clone, Copy, Debug)]
pub(super) struct Sample {
    pub host: Instant,
    pub network: u64,
    pub round_trip: f64,
}

impl Sample {
    pub fn exchange(
        t1: u64,
        t2: Instant,
        t3: Instant,
        t4: u64,
        forward_correction: i128,
        reverse_correction: i64,
        rate: f64,
    ) -> Option<Self> {
        let elapsed = signed_nanos(t3, t2);
        if !(0..=1_000_000_000).contains(&elapsed) {
            return None;
        }
        let forward = i128::from(t1) * 65536 + forward_correction;
        let reverse = i128::from(t4) * 65536 - i128::from(reverse_correction);
        let round_trip = (reverse - forward) as f64 / 65536.0 - elapsed as f64 * rate;
        if !(0.0..=200_000_000.0).contains(&round_trip) {
            return None;
        }
        let network = (forward + reverse) / (2 * 65536);
        Some(Self {
            host: t2 + Duration::from_nanos(elapsed as u64 / 2),
            network: u64::try_from(network).ok()?,
            round_trip,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RateSource {
    Assumed,
    Measured,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Quality {
    pub offset_ns: f64,
    pub asymmetry_ns: f64,
    pub rate_ppm: f64,
    pub samples: usize,
    pub rate_source: RateSource,
}

pub(super) struct Servo {
    timeline: Timeline,
    samples: VecDeque<Sample>,
    estimate: Option<Timebase>,
    pub quality: Option<Quality>,
    pub last_good: Option<Instant>,
    indicator: Option<u16>,
    step: Vec<Sample>,
}

impl Servo {
    pub fn new(clock: ClockIdentity, epoch: u32) -> Self {
        Self {
            timeline: Timeline { clock, epoch },
            samples: VecDeque::new(),
            estimate: None,
            quality: None,
            last_good: None,
            indicator: None,
            step: Vec::new(),
        }
    }

    pub fn timebase(&self) -> Option<Timebase> {
        self.estimate
    }
    pub fn timeline(&self) -> Timeline {
        self.timeline
    }
    pub fn rate(&self) -> f64 {
        self.estimate.map_or(1.0, |e| e.rate)
    }

    pub fn indicator(&mut self, indicator: u16) {
        if self.indicator.is_some_and(|old| old != indicator) {
            self.reset();
        }
        self.indicator = Some(indicator);
    }

    fn reset(&mut self) {
        self.timeline.epoch = self.timeline.epoch.wrapping_add(1);
        self.samples.clear();
        self.step.clear();
        self.estimate = None;
        self.quality = None;
        self.last_good = None;
    }

    pub fn observe(&mut self, sample: Sample) -> Result<(), &'static str> {
        if self
            .samples
            .back()
            .is_some_and(|last| sample.host <= last.host)
        {
            return Ok(());
        }
        let minimum = self
            .samples
            .iter()
            .map(|s| s.round_trip)
            .fold(sample.round_trip, f64::min);
        let good_delay =
            sample.round_trip <= minimum + minimum.clamp(MIN_DELAY_MARGIN_NS, MAX_DELAY_MARGIN_NS);
        if good_delay && let Some(estimate) = self.estimate {
            let residual = i128::from(sample.network)
                - i128::from(
                    estimate
                        .network(sample.host)
                        .ok_or("PTP network time overflow")?
                        .nanos,
                );
            if residual.abs() > PHASE_STEP_NS {
                if self.step.last().is_some_and(|last| {
                    let previous = i128::from(last.network)
                        - i128::from(estimate.network(last.host).unwrap().nanos);
                    (residual - previous).abs() > 500_000
                }) {
                    self.step.clear();
                }
                self.step.push(sample);
                if self.step.len() >= 3 {
                    let pending = std::mem::take(&mut self.step);
                    self.reset();
                    self.samples.extend(pending);
                }
                return Ok(());
            }
            self.step.clear();
        }
        self.samples.push_back(sample);
        while self.samples.len() > WINDOW
            || self
                .samples
                .front()
                .is_some_and(|s| sample.host - s.host > Duration::from_secs(16))
        {
            self.samples.pop_front();
        }
        if self.samples.len() < 8 || sample.host - self.samples[0].host < Duration::from_secs(1) {
            return Ok(());
        }
        let mut selected: Vec<_> = self.samples.iter().copied().collect();
        selected.sort_by(|a, b| {
            a.round_trip
                .total_cmp(&b.round_trip)
                .then_with(|| b.host.cmp(&a.host))
        });
        let minimum = selected[0].round_trip;
        selected.truncate(selected.len().div_ceil(2));
        selected.sort_by_key(|s| s.host);
        let (mut rate, rate_source) = match (self.estimate, self.quality) {
            (Some(estimate), Some(quality)) if quality.rate_source == RateSource::Measured => {
                (estimate.rate, RateSource::Measured)
            }
            _ => (1.0, RateSource::Assumed),
        };
        let Some(prior) = projected(&selected, sample, minimum, rate, rate_source) else {
            return Ok(());
        };
        let mut network = prior.network;
        let mut quality = prior.quality;
        if let Some(fitted) = measured(selected, sample, minimum)
            && fitted.quality.rate_ppm < RATE_PRIOR_UNCERTAINTY * 1e6
        {
            if (fitted.rate - 1.0).abs() > 0.001 {
                return Ok(());
            }
            if fitted.quality.offset_ns <= MAX_OFFSET_UNCERTAINTY_NS {
                network = fitted.network;
                rate = fitted.rate;
                quality = fitted.quality;
            }
        }
        if quality.offset_ns > MAX_OFFSET_UNCERTAINTY_NS {
            return Ok(());
        }
        let network = u64::try_from(network).map_err(|_| "PTP fitted time overflow")?;
        self.estimate = Timebase::new(self.timeline, sample.host, PtpTime { nanos: network }, rate);
        self.quality = Some(quality);
        self.last_good = Some(sample.host);
        Ok(())
    }
}

struct Fitted {
    network: i128,
    rate: f64,
    quality: Quality,
}

fn projected(
    selected: &[Sample],
    sample: Sample,
    minimum: f64,
    rate: f64,
    rate_source: RateSource,
) -> Option<Fitted> {
    let reference = *selected.first()?;
    let mut points: Vec<_> = selected
        .iter()
        .map(|s| {
            let host = x(*s, reference);
            let offset =
                (i128::from(s.network) - i128::from(reference.network)) as f64 - rate * host;
            (host, offset)
        })
        .collect();
    let center = median(points.iter().map(|(_, offset)| *offset).collect());
    let mad = median(
        points
            .iter()
            .map(|(_, offset)| (*offset - center).abs())
            .collect(),
    );
    points.retain(|(_, offset)| (*offset - center).abs() <= (4.5 * mad).max(20_000.0));
    if points.len() < 4 {
        return None;
    }
    let count = points.len() as f64;
    let mean_offset = points.iter().map(|(_, offset)| offset).sum::<f64>() / count;
    let mean_host = points.iter().map(|(host, _)| host).sum::<f64>() / count;
    let scatter = (points
        .iter()
        .map(|(_, offset)| (*offset - mean_offset).powi(2))
        .sum::<f64>()
        / (count - 1.0))
        .sqrt()
        .max(1_000.0);
    let at = x(sample, reference);
    Some(Fitted {
        network: i128::from(reference.network) + (rate * at + mean_offset).round() as i128,
        rate,
        quality: Quality {
            offset_ns: 3.0 * scatter / count.sqrt()
                + (at - mean_host).abs() * RATE_PRIOR_UNCERTAINTY,
            asymmetry_ns: minimum / 2.0,
            rate_ppm: RATE_PRIOR_UNCERTAINTY * 1e6,
            samples: points.len(),
            rate_source,
        },
    })
}

fn measured(mut selected: Vec<Sample>, sample: Sample, minimum: f64) -> Option<Fitted> {
    if selected.len() < 4 {
        return None;
    }
    let reference = selected[0];
    let (intercept, slope, _, _) = fit(&selected, reference);
    let residuals: Vec<_> = selected
        .iter()
        .map(|s| y(*s, reference) - intercept - slope * x(*s, reference))
        .collect();
    let center = median(residuals.clone());
    let mad = median(residuals.iter().map(|r| (r - center).abs()).collect());
    let bound = (4.5 * mad).max(20_000.0);
    selected.retain(|s| {
        (y(*s, reference) - intercept - slope * x(*s, reference) - center).abs() <= bound
    });
    if selected.len() < 4 || selected.last()?.host - selected[0].host < Duration::from_millis(250) {
        return None;
    }
    let (intercept, slope, scatter, sum_xx) = fit(&selected, reference);
    let count = selected.len() as f64;
    let at = x(sample, reference);
    let mean_x = selected.iter().map(|s| x(*s, reference)).sum::<f64>() / count;
    let noise = scatter.max(1_000.0);
    let rate_error = 3.0 * noise / sum_xx.sqrt();
    let offset_error = 3.0 * noise * (1.0 / count + (at - mean_x).powi(2) / sum_xx).sqrt();
    if !slope.is_finite() || !offset_error.is_finite() || !rate_error.is_finite() {
        return None;
    }
    Some(Fitted {
        network: i128::from(reference.network) + (at + intercept + slope * at).round() as i128,
        rate: 1.0 + slope,
        quality: Quality {
            offset_ns: offset_error,
            asymmetry_ns: minimum / 2.0,
            rate_ppm: rate_error * 1e6,
            samples: selected.len(),
            rate_source: RateSource::Measured,
        },
    })
}

fn x(sample: Sample, reference: Sample) -> f64 {
    signed_nanos(sample.host, reference.host) as f64
}
fn y(sample: Sample, reference: Sample) -> f64 {
    (i128::from(sample.network) - i128::from(reference.network)) as f64 - x(sample, reference)
}
fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}
fn fit(samples: &[Sample], reference: Sample) -> (f64, f64, f64, f64) {
    let count = samples.len() as f64;
    let mx = samples.iter().map(|s| x(*s, reference)).sum::<f64>() / count;
    let my = samples.iter().map(|s| y(*s, reference)).sum::<f64>() / count;
    let xx = samples
        .iter()
        .map(|s| (x(*s, reference) - mx).powi(2))
        .sum::<f64>();
    let xy = samples
        .iter()
        .map(|s| (x(*s, reference) - mx) * (y(*s, reference) - my))
        .sum::<f64>();
    let slope = xy / xx;
    let intercept = my - slope * mx;
    let variance = samples
        .iter()
        .map(|s| (y(*s, reference) - intercept - slope * x(*s, reference)).powi(2))
        .sum::<f64>()
        / (count - 2.0);
    (intercept, slope, variance.sqrt(), xx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn truth(
        servo: &mut Servo,
        host: Instant,
        index: u64,
        rate: f64,
        offset: u64,
        forward: u64,
        reverse: u64,
    ) {
        let t1 = host + Duration::from_millis(index * 125);
        let t2 = t1 + Duration::from_nanos(forward);
        let t3 = t2 + Duration::from_micros(40);
        let t4 = t3 + Duration::from_nanos(reverse);
        let network = |at| offset + (signed_nanos(at, host) as f64 * rate).round() as u64;
        servo
            .observe(
                Sample::exchange(network(t1), t2, t3, network(t4), 0, 0, servo.rate()).unwrap(),
            )
            .unwrap();
    }

    #[test]
    fn synthetic_offset_rate_jitter_and_asymmetric_spikes() {
        for rate in [0.9999, 1.0001] {
            let host = Instant::now();
            let mut servo = Servo::new(ClockIdentity([2; 8]), 0);
            let mut first_settled = None;
            for index in 0..240 {
                let jitter = (index * 7919 % 30_000) * 2;
                let forward = 100_000 + jitter + if index % 11 == 0 { 20_000_000 } else { 0 };
                let reverse = 100_000
                    + (index * 3571 % 30_000) * 2
                    + if index % 17 == 0 { 15_000_000 } else { 0 };
                truth(
                    &mut servo,
                    host,
                    index,
                    rate,
                    231_017_000_000_000,
                    forward,
                    reverse,
                );
                if servo.timebase().is_some() {
                    first_settled.get_or_insert(index);
                }
            }
            let estimate = servo.timebase().unwrap();
            let at = host + Duration::from_secs(30);
            let error = i128::from(estimate.network(at).unwrap().nanos)
                - (231_017_000_000_000 + (30e9 * rate) as i128);
            eprintln!(
                "synthetic rate={rate}: first={}ms, offset={error}ns, rate_error={}ppm, quality={:?}",
                first_settled.unwrap() * 125,
                (estimate.rate - rate) * 1e6,
                servo.quality
            );
            assert!(first_settled.unwrap() <= 24);
            assert!(error.abs() < 20_000);
            assert!((estimate.rate - rate).abs() < 3e-6);
            assert_eq!(estimate.timeline.epoch, 0);
        }
    }

    #[test]
    fn constant_asymmetry_leaves_half_the_delay_difference() {
        let host = Instant::now();
        let mut servo = Servo::new(ClockIdentity([2; 8]), 0);
        for i in 0..64 {
            truth(&mut servo, host, i, 1.0, 5_000_000_000, 300_000, 100_000);
        }
        let estimate = servo.timebase().unwrap();
        let error = i128::from(estimate.network(host).unwrap().nanos) - 5_000_000_000;
        assert_eq!(error, -100_000);
        let quality = servo.quality.unwrap();
        assert!(quality.offset_ns < 10_000.0);
        assert_eq!(quality.asymmetry_ns, 200_000.0);
        assert!(quality.asymmetry_ns >= error.abs() as f64);
    }

    #[test]
    fn every_correction_and_fraction_is_in_the_e2e_equation() {
        let host = Instant::now();
        let sample = Sample::exchange(
            1_000_000_000,
            host,
            host + Duration::from_micros(40),
            1_000_500_000,
            100_000 * 65536 + 32768,
            120_000 * 65536 + 32768,
            1.0,
        )
        .unwrap();
        assert_eq!(sample.host, host + Duration::from_micros(20));
        assert_eq!(sample.network, 1_000_240_000);
        assert_eq!(sample.round_trip, 239_999.0);
    }

    #[test]
    fn phase_steps_and_indicator_changes_invalidate_the_timeline() {
        let host = Instant::now();
        let mut servo = Servo::new(ClockIdentity([2; 8]), 0);
        for i in 0..32 {
            truth(&mut servo, host, i, 1.0, 5_000_000_000, 100_000, 100_000);
        }
        for i in 32..35 {
            truth(&mut servo, host, i, 1.0, 8_000_000_000, 100_000, 100_000);
        }
        assert!(servo.timebase().is_none());
        assert_eq!(servo.timeline().epoch, 1);
        for i in 35..64 {
            truth(&mut servo, host, i, 1.0, 8_000_000_000, 100_000, 100_000);
        }
        assert_eq!(servo.timebase().unwrap().timeline.epoch, 1);
        servo.indicator(2);
        assert!(servo.timebase().is_some());
        servo.indicator(3);
        assert!(servo.timebase().is_none());
        assert_eq!(servo.timeline().epoch, 2);
    }

    #[test]
    fn stable_long_paths_settle_with_asymmetry_reported_and_invalid_rates_do_not() {
        let host = Instant::now();
        let mut servo = Servo::new(ClockIdentity([2; 8]), 0);
        for i in 0..64 {
            truth(
                &mut servo,
                host,
                i,
                1.0,
                5_000_000_000,
                10_000_000,
                10_000_000,
            );
        }
        assert!(servo.timebase().is_some());
        assert!(servo.quality.unwrap().offset_ns < 10_000.0);
        assert_eq!(servo.quality.unwrap().asymmetry_ns, 10_000_000.0);
        let mut servo = Servo::new(ClockIdentity([2; 8]), 0);
        for i in 0..64 {
            truth(&mut servo, host, i, 1.01, 5_000_000_000, 100_000, 100_000);
        }
        assert!(servo.timebase().is_none());
    }
    #[test]
    fn inconsistent_low_delay_observations_do_not_settle() {
        let host = Instant::now();
        let mut servo = Servo::new(ClockIdentity([2; 8]), 0);
        for index in 0..128_u64 {
            let offset = (i128::from(index * 7 % 17) - 8) * 10_000_000;
            let network = 1_000_000_000_000_i128 + i128::from(index) * 125_000_000 + offset;
            servo
                .observe(Sample {
                    host: host + Duration::from_millis(index * 125),
                    network: network as u64,
                    round_trip: 190_000_000.0,
                })
                .unwrap();
            assert!(servo.timebase().is_none());
        }
    }

    #[test]
    fn wifi_delay_variation_and_spikes_settle_against_known_truth() {
        for rate in [0.9999, 1.0001] {
            let host = Instant::now();
            let mut servo = Servo::new(ClockIdentity([2; 8]), 0);
            let mut seed = 0x1234_5678_u64;
            let mut jitter = || {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                (seed >> 32) % 3_000_000
            };
            let mut settled = None;
            for i in 0..480 {
                let forward = 1_500_000 + jitter() + if i % 11 == 0 { 50_000_000 } else { 0 };
                let reverse = 1_500_000 + jitter() + if i % 17 == 0 { 20_000_000 } else { 0 };
                truth(
                    &mut servo,
                    host,
                    i,
                    rate,
                    231_017_000_000_000,
                    forward,
                    reverse,
                );
                if let Some(estimate) = servo.timebase() {
                    settled.get_or_insert(i);
                    let at = host + Duration::from_millis(i * 125);
                    let expected =
                        231_017_000_000_000 + (i as f64 * 125_000_000.0 * rate).round() as i128;
                    let error = i128::from(estimate.network(at).unwrap().nanos) - expected;
                    assert!(error.abs() as f64 <= MAX_OFFSET_UNCERTAINTY_NS);
                    assert!((estimate.rate - rate).abs() <= RATE_PRIOR_UNCERTAINTY);
                    let quality = servo.quality.unwrap();
                    for seconds in [2, 15] {
                        let future = at + Duration::from_secs(seconds);
                        let expected = 231_017_000_000_000
                            + ((i as f64 * 125_000_000.0 + seconds as f64 * 1e9) * rate).round()
                                as i128;
                        let error = i128::from(estimate.network(future).unwrap().nanos) - expected;
                        let bound = quality.offset_ns
                            + quality.asymmetry_ns
                            + seconds as f64 * quality.rate_ppm * 1000.0;
                        assert!(error.abs() as f64 <= bound);
                    }
                }
            }
            let estimate = servo.timebase().unwrap();
            let now = host + Duration::from_secs(60);
            let error = i128::from(estimate.network(now).unwrap().nanos)
                - (231_017_000_000_000 + (60e9 * rate).round() as i128);
            let quality = servo.quality.unwrap();
            eprintln!(
                "synthetic Wi-Fi rate={rate}: settled={}ms offset={error}ns rate_error={}ppm quality={quality:?}",
                settled.unwrap() * 125,
                (estimate.rate - rate) * 1e6
            );
            assert!(settled.unwrap() <= 64);
            assert!(error.abs() < 500_000);
            assert!((estimate.rate - rate).abs() < 50e-6);
            assert!(error.abs() as f64 <= quality.offset_ns + quality.asymmetry_ns);
            assert!((estimate.rate - rate).abs() * 1e6 <= quality.rate_ppm);
            assert_eq!(servo.timeline().epoch, 0);
        }
    }
}

#[cfg(test)]
mod captured_path {
    use super::*;

    #[test]
    fn soundbar_capture_settles_with_measured_wifi_delays() {
        for family in [4_u64, 6] {
            let origin = Instant::now();
            let mut servo = Servo::new(ClockIdentity([2; 8]), 0);
            let mut first_packet = None;
            let mut first_settled = None;
            let mut initial_quality = None;
            for line in include_str!("capture.csv").lines() {
                let values: Vec<i128> = line.split(',').map(|x| x.parse().unwrap()).collect();
                if values[0] != i128::from(family) {
                    continue;
                }
                let t2 = origin + Duration::from_nanos(values[2] as u64);
                let t3 = origin + Duration::from_nanos(values[3] as u64);
                first_packet.get_or_insert(t2);
                if let Some(sample) = Sample::exchange(
                    values[1] as u64,
                    t2,
                    t3,
                    values[4] as u64,
                    values[5] + values[6],
                    values[7] as i64,
                    servo.rate(),
                ) {
                    servo.observe(sample).unwrap();
                    if let Some(timebase) = servo.timebase() {
                        if first_settled.is_none() {
                            initial_quality = servo.quality;
                            assert_eq!(timebase.rate, 1.0);
                        }
                        first_settled.get_or_insert(t2);
                    }
                }
            }
            let settling = first_settled.unwrap().duration_since(first_packet.unwrap());
            eprintln!(
                "capture IPv{family}: settling={}ms final_quality={:?}",
                settling.as_millis(),
                servo.quality
            );
            assert!(settling < Duration::from_secs(2));
            assert_eq!(initial_quality.unwrap().rate_source, RateSource::Assumed);
            assert_eq!(initial_quality.unwrap().rate_ppm, 100.0);
            assert_eq!(servo.quality.unwrap().rate_source, RateSource::Measured);
            eprintln!(
                "capture IPv{family} initial quality: {:?}",
                initial_quality.unwrap()
            );
            assert_eq!(servo.timeline().epoch, 0);
            assert!(servo.quality.unwrap().offset_ns < MAX_OFFSET_UNCERTAINTY_NS);
        }
    }
}
