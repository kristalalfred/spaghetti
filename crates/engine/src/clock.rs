use std::sync::{Mutex, MutexGuard};

use crate::host_time;

/// Claude: a round trip slower than this says more about the network than
/// about the two clocks.
const MAX_ROUND_TRIP: f64 = 0.1;
/// Claude: how far the offset wanders on top of its drift, as the variance
/// its random walk gains per second.
const WANDER: f64 = 1e-10;
/// Claude: once the estimate is trusted, a residual beyond this share of its
/// round trip's bound is a step in the offset, and the estimate loosens by
/// `LOOSEN` so that it follows the step instead of averaging it away.
const STEP: f64 = 0.75;
const LOOSEN: f64 = 1.002;
const TRUSTED_SAMPLES: u32 = 100;
/// Claude: the shortest span between the first two samples to read drift from.
const DRIFT_BASELINE: f64 = 0.1;
/// Claude: drift applies once it stands this many deviations clear of zero.
const DRIFT_SIGNIFICANCE: f64 = 2.0;
/// Claude: no two crystals drift this far apart; an estimate past it diverged.
const MAX_DRIFT: f64 = 0.001;

const PROBE: u8 = 1;
const REPLY: u8 = 2;

/// Claude: a remote clock as an offset from this host's and the drift of that
/// offset, estimated from NTP-style round trips by a two-state Kalman filter.
/// Its model follows the time filter of the Sendspin protocol: a round trip
/// measures the offset to within half its length, and the offset wanders as a
/// random walk on top of a steady drift. Times are in seconds.
#[derive(Debug, Clone, Default)]
pub struct ClockFilter {
    samples: u32,
    /// Claude: local time of the latest sample.
    at: f64,
    /// Claude: the remote clock's lead over the local one at `at`.
    offset: f64,
    drift: f64,
    offset_variance: f64,
    covariance: f64,
    drift_variance: f64,
}

impl ClockFilter {
    /// Claude: one round trip: a probe `sent` and its reply `received` on the
    /// local clock, and the probe received and the reply sent on the remote's.
    /// Returns whether the round trip was usable.
    pub fn record(
        &mut self,
        sent: f64,
        remote_received: f64,
        remote_sent: f64,
        received: f64,
    ) -> bool {
        let round_trip = (received - sent) - (remote_sent - remote_received);
        let at = (sent + received) / 2.0;
        if !(0.0..=MAX_ROUND_TRIP).contains(&round_trip) || (self.samples > 0 && at <= self.at) {
            return false;
        }
        let measured = ((remote_received - sent) + (remote_sent - received)) / 2.0;
        let bound = (round_trip / 2.0).max(1e-6);
        let noise = bound * bound;
        let elapsed = at - self.at;
        self.at = at;
        match self.samples {
            0 => {
                self.offset = measured;
                self.offset_variance = noise;
            }
            1 => {
                let baseline = elapsed.max(DRIFT_BASELINE);
                if elapsed >= DRIFT_BASELINE {
                    self.drift = (measured - self.offset) / elapsed;
                }
                self.drift_variance = (self.offset_variance + noise) / (baseline * baseline);
                self.offset = measured;
                self.offset_variance = noise;
            }
            _ => {
                let predicted = self.offset + self.drift * elapsed;
                let residual = measured - predicted;
                let mut offset_variance = self.offset_variance
                    + 2.0 * self.covariance * elapsed
                    + self.drift_variance * elapsed * elapsed
                    + WANDER * elapsed;
                let mut covariance = self.covariance + self.drift_variance * elapsed;
                let mut drift_variance = self.drift_variance;
                if self.samples >= TRUSTED_SAMPLES && residual.abs() > STEP * bound {
                    offset_variance *= LOOSEN;
                    covariance *= LOOSEN;
                    drift_variance *= LOOSEN;
                }
                let innovation = offset_variance + noise;
                let offset_gain = offset_variance / innovation;
                let drift_gain = covariance / innovation;
                self.offset = predicted + offset_gain * residual;
                self.drift += drift_gain * residual;
                self.offset_variance = offset_variance - offset_gain * offset_variance;
                self.covariance = covariance - offset_gain * covariance;
                self.drift_variance = drift_variance - drift_gain * covariance;
            }
        }
        self.samples = (self.samples + 1).min(TRUSTED_SAMPLES);
        true
    }

    pub fn is_known(&self) -> bool {
        self.samples >= 2
    }

    fn applied_drift(&self) -> f64 {
        let significant = self.drift * self.drift
            >= DRIFT_SIGNIFICANCE * DRIFT_SIGNIFICANCE * self.drift_variance;
        if significant && self.drift.abs() <= MAX_DRIFT {
            self.drift
        } else {
            0.0
        }
    }

    /// Claude: the local time when the remote clock reads `remote`.
    pub fn local(&self, remote: f64) -> Option<f64> {
        let drift = self.applied_drift();
        self.is_known()
            .then(|| (remote - self.offset + drift * self.at) / (1.0 + drift))
    }
}

/// Claude: when the sender captured one point of its stream: the RTP time of a
/// packet it sent and the host time, in nanoseconds, of its first frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capture {
    pub rtp_time: u64,
    pub captured: u64,
}

/// Claude: what a link's clock channel carries. The receiving end probes, and
/// the sending end replies with its own clock and its latest capture. Times are
/// host nanoseconds of the end that took them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockMessage {
    Probe {
        sent: u64,
    },
    Reply {
        probe: u64,
        received: u64,
        sent: u64,
        latest: Option<Capture>,
    },
}

impl ClockMessage {
    pub fn encode(&self) -> Vec<u8> {
        let (kind, fields) = match *self {
            Self::Probe { sent } => (PROBE, vec![sent]),
            Self::Reply {
                probe,
                received,
                sent,
                latest,
            } => {
                let mut fields = vec![probe, received, sent];
                if let Some(latest) = latest {
                    fields.extend([latest.rtp_time, latest.captured]);
                }
                (REPLY, fields)
            }
        };
        let mut bytes = vec![kind];
        for field in fields {
            bytes.extend(field.to_le_bytes());
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (&kind, rest) = bytes.split_first()?;
        if rest.len() % 8 != 0 {
            return None;
        }
        let fields: Vec<u64> = rest
            .as_chunks::<8>()
            .0
            .iter()
            .map(|chunk| u64::from_le_bytes(*chunk))
            .collect();
        match (kind, fields.as_slice()) {
            (PROBE, &[sent]) => Some(Self::Probe { sent }),
            (REPLY, &[probe, received, sent, ref latest @ ..]) => {
                let latest = match *latest {
                    [] => None,
                    [rtp_time, captured] => Some(Capture { rtp_time, captured }),
                    _ => return None,
                };
                Some(Self::Reply {
                    probe,
                    received,
                    sent,
                    latest,
                })
            }
            _ => None,
        }
    }
}

pub fn now_nanos() -> u64 {
    host_time::to_nanos(host_time::now())
}

fn seconds(nanos: u64) -> f64 {
    nanos as f64 * 1e-9
}

/// Claude: what a receiving link knows of its sender's clock, fed by the
/// link's session and read by the source that stamps frames with it.
#[derive(Default)]
pub struct SenderClock(Mutex<Known>);

#[derive(Default)]
struct Known {
    filter: ClockFilter,
    latest: Option<Capture>,
}

impl SenderClock {
    fn known(&self) -> MutexGuard<'_, Known> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Claude: a reply to the probe this host sent, which arrived at `received`.
    pub fn record(&self, reply: ClockMessage, received: u64) {
        let ClockMessage::Reply {
            probe,
            received: remote_received,
            sent: remote_sent,
            latest,
        } = reply
        else {
            return;
        };
        let mut known = self.known();
        known.filter.record(
            seconds(probe),
            seconds(remote_received),
            seconds(remote_sent),
            seconds(received),
        );
        if latest.is_some() {
            known.latest = latest;
        }
    }

    /// Claude: the local host time, in ticks, at which the sender captured the
    /// frame at `rtp_time`, counted at `rtp_rate`, once its clock is known. RTP
    /// times compare in 32 bits, as the packets carry them.
    pub fn captured(&self, rtp_time: u64, rtp_rate: f64) -> Option<u64> {
        let known = self.known();
        let latest = known.latest?;
        let frames = (rtp_time as u32).wrapping_sub(latest.rtp_time as u32) as i32;
        let remote = seconds(latest.captured) + f64::from(frames) / rtp_rate;
        let local = known.filter.local(remote)?;
        (local > 0.0).then(|| host_time::from_nanos((local * 1e9) as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude: a remote clock that leads by `offset` seconds and runs `drift`
    /// fast, probed every `interval` over a path of `one_way` each way plus
    /// up to `jitter` more, spread by a fixed sequence.
    struct Path {
        offset: f64,
        drift: f64,
        one_way: f64,
        jitter: f64,
    }

    impl Path {
        fn remote(&self, local: f64) -> f64 {
            local + self.offset + self.drift * local
        }

        fn feed(&self, filter: &mut ClockFilter, from: f64, probes: usize, interval: f64) {
            let mut spread = 0x2545_f491_4f6c_dd1du64;
            let mut next = || {
                spread ^= spread << 13;
                spread ^= spread >> 7;
                spread ^= spread << 17;
                (spread >> 11) as f64 / (1u64 << 53) as f64
            };
            for i in 0..probes {
                let sent = from + i as f64 * interval;
                let arrives = sent + self.one_way + self.jitter * next();
                let replied = arrives + 0.0001;
                let received = replied + self.one_way + self.jitter * next();
                filter.record(sent, self.remote(arrives), self.remote(replied), received);
            }
        }

        fn error_at(&self, filter: &ClockFilter, local: f64) -> f64 {
            filter.local(self.remote(local)).unwrap() - local
        }
    }

    #[test]
    fn finds_the_offset_within_its_jitter() {
        let path = Path {
            offset: 1234.5,
            drift: 0.0,
            one_way: 0.002,
            jitter: 0.004,
        };
        let mut filter = ClockFilter::default();
        path.feed(&mut filter, 100.0, 60, 0.5);
        let error = path.error_at(&filter, 130.0);
        assert!(error.abs() < 0.0005, "off by {error}");
    }

    #[test]
    fn follows_drift_between_probes() {
        let path = Path {
            offset: -50.0,
            drift: 80e-6,
            one_way: 0.001,
            jitter: 0.002,
        };
        let mut filter = ClockFilter::default();
        path.feed(&mut filter, 1000.0, 600, 1.0);
        let error = path.error_at(&filter, 1610.0);
        assert!(
            error.abs() < 0.0003,
            "off by {error} 10 s past the last probe"
        );
    }

    #[test]
    fn ignores_round_trips_too_slow_or_inconsistent() {
        let mut filter = ClockFilter::default();
        assert!(!filter.record(10.0, 20.0, 20.0, 10.2));
        assert!(!filter.record(10.0, 20.0, 20.1, 10.05));
        assert!(!filter.is_known());
        assert!(filter.record(10.0, 20.0, 20.0, 10.004));
        assert!(
            !filter.record(9.0, 19.0, 19.0, 9.004),
            "an older round trip"
        );
        assert!(filter.record(10.5, 20.5, 20.5, 10.504));
        assert!(filter.is_known());
    }

    #[test]
    fn messages_round_trip_through_bytes() {
        let messages = [
            ClockMessage::Probe { sent: 7 },
            ClockMessage::Reply {
                probe: 7,
                received: 1 << 40,
                sent: (1 << 40) + 5,
                latest: None,
            },
            ClockMessage::Reply {
                probe: 7,
                received: 8,
                sent: 9,
                latest: Some(Capture {
                    rtp_time: 960,
                    captured: u64::MAX,
                }),
            },
        ];
        for message in messages {
            assert_eq!(ClockMessage::decode(&message.encode()), Some(message));
        }
        assert_eq!(ClockMessage::decode(&[]), None);
        assert_eq!(ClockMessage::decode(&[PROBE, 1, 2]), None);
        assert_eq!(ClockMessage::decode(&[9; 9]), None);
    }

    #[test]
    fn maps_rtp_time_across_the_32_bit_wrap() {
        let clock = SenderClock::default();
        let latest = Capture {
            rtp_time: u64::from(u32::MAX) - 479,
            captured: 50_000_000_000,
        };
        for i in 0..2u64 {
            let probe = 10_000_000_000 + i * 500_000_000;
            let reply = ClockMessage::Reply {
                probe,
                received: probe + 40_000_000_000 + 1_000_000,
                sent: probe + 40_000_000_000 + 1_000_000,
                latest: Some(latest),
            };
            clock.record(reply, probe + 2_000_000);
        }
        let ticks = clock.captured(480, 48000.0).expect("the clock is known");
        let local = host_time::to_nanos(ticks) as f64 * 1e-9;
        assert!((local - 10.02).abs() < 1e-6, "captured at {local}");
    }
}
