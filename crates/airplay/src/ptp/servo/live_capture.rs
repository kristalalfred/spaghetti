use super::*;
use std::collections::HashMap;

struct Exchange {
    t1: u64,
    t2: u64,
    t3: u64,
    t4: u64,
    corrections: [i64; 3],
    indicator: u16,
}

fn host_time(value: &str) -> u64 {
    let (seconds, nanos) = value.split_once('.').unwrap();
    assert_eq!(nanos.len(), 9);
    seconds.parse::<u64>().unwrap() * 1_000_000_000 + nanos.parse::<u64>().unwrap()
}

fn exchanges(csv: &str, receiver: &str) -> Vec<Exchange> {
    let mut sync = HashMap::new();
    let mut follow = HashMap::new();
    let mut requests = HashMap::new();
    let mut responses = HashMap::new();
    for line in csv.lines().filter(|line| !line.contains('\\')) {
        let fields: Vec<_> = line.split(',').collect();
        assert!((10..=11).contains(&fields.len()));
        let host = host_time(fields[0]);
        let incoming = fields[1] == receiver;
        let kind = u8::from_str_radix(fields[2].trim_start_matches("0x"), 16).unwrap();
        let sequence = fields[3].parse::<u16>().unwrap();
        let correction = fields[4].parse::<i64>().unwrap();
        let timestamp = |index: usize| {
            fields[index].parse::<u64>().unwrap() * 1_000_000_000
                + fields[index + 1].parse::<u64>().unwrap()
        };
        match (incoming, kind) {
            (true, 0) => {
                sync.insert(sequence, (host, correction));
            }
            (true, 8) => {
                follow.insert(
                    sequence,
                    (timestamp(5), correction, fields[9].parse::<u16>().unwrap()),
                );
            }
            (false, 1) => {
                requests.insert(sequence, host);
            }
            (true, 9) => {
                responses.insert(sequence, (timestamp(7), correction));
            }
            _ => (),
        }
    }
    let mut result: Vec<_> = sync
        .into_iter()
        .filter_map(|(sequence, (t2, sync_correction))| {
            let request = sequence.wrapping_add(1);
            let &(t1, follow_correction, indicator) = follow.get(&sequence)?;
            let &t3 = requests.get(&request)?;
            let &(t4, response_correction) = responses.get(&request)?;
            Some(Exchange {
                t1,
                t2,
                t3,
                t4,
                corrections: [sync_correction, follow_correction, response_correction],
                indicator,
            })
        })
        .collect();
    result.sort_by_key(|exchange| exchange.t2);
    result
}

#[test]
fn live_soundbar_timestamps_have_nonnegative_rtt_and_bounded_low_delay_spread() {
    for (name, csv, receiver, count, spread_bound) in [
        (
            "IPv6",
            include_str!("../testdata/live1.csv"),
            "fe80::d85c:c0f5:c1a0:d348",
            353,
            1_300_000.0,
        ),
        (
            "IPv4",
            include_str!("../testdata/live2.csv"),
            "192.168.3.57",
            1462,
            900_000.0,
        ),
    ] {
        let exchanges = exchanges(csv, receiver);
        assert_eq!(exchanges.len(), count);
        assert!(
            exchanges
                .iter()
                .any(|e| e.corrections.iter().any(|c| c.abs() > 30_000_000))
        );
        let start = exchanges[0].t2;
        let origin = Instant::now();
        let mut servo = Servo::new(ClockIdentity([2; 8]), 0);
        let mut first_settled = None;
        let mut offsets = Vec::new();
        let mut delays = Vec::new();
        let mut discarded = 0;
        for e in &exchanges {
            let t2 = origin + Duration::from_nanos(e.t2 - start);
            let t3 = origin + Duration::from_nanos(e.t3 - start);
            let rtt = i128::from(e.t4) - i128::from(e.t1) - i128::from(e.t3 - e.t2);
            assert!(rtt >= 0, "{name}: negative RTT at {}", e.t2 - start);
            delays.push(rtt);
            offsets.push(
                i128::from(e.t1) + i128::from(e.t4)
                    - i128::from(e.t2 - start)
                    - i128::from(e.t3 - start),
            );
            if rtt > 200_000_000 {
                discarded += 1;
                continue;
            }
            let sample = Sample::exchange(e.t1, t2, t3, e.t4, servo.rate()).unwrap();
            servo.indicator(e.indicator);
            servo.observe(sample).unwrap();
            if servo.timebase().is_some() {
                first_settled.get_or_insert((e.t2 - start, servo.quality.unwrap()));
            }
        }
        let mut sorted_offsets = offsets.clone();
        sorted_offsets.sort();
        let center = (sorted_offsets[(count - 1) / 2] + sorted_offsets[count / 2]) / 2;
        let mut sorted_delays = delays.clone();
        sorted_delays.sort();
        let threshold = sorted_delays[count / 4];
        let spread = offsets
            .iter()
            .zip(delays)
            .filter(|(_, delay)| *delay <= threshold)
            .map(|(offset, _)| (offset - center).abs())
            .max()
            .unwrap() as f64
            / 2.0;
        assert!(
            spread <= spread_bound,
            "{name}: low-delay spread {spread} ns"
        );
        let (settling, initial_quality) = first_settled.unwrap();
        assert!(settling < 2_000_000_000, "{name}: settling {settling} ns");
        assert!(initial_quality.offset_ns <= MAX_OFFSET_UNCERTAINTY_NS);
        assert_eq!(servo.timeline().epoch, 0);
        assert_eq!(servo.quality.unwrap().rate_source, RateSource::Measured);
        eprintln!(
            "live {name}: n={count}, min_rtt={}ns, low_delay_spread={spread}ns, over_200ms={discarded}, settling={settling}ns, initial={initial_quality:?}, final={:?}",
            sorted_delays[0], servo.quality
        );
    }
}
