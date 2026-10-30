use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use coreaudio_sys::{AudioGetCurrentHostTime, AudioGetHostClockFrequency};
use spaghetti_common::ipc_protocol::{FlowState, FlowStatus};

use crate::flow::{FlowMeter, Link};
use crate::mixer::{Cycles, Fader};
use crate::playout::{Playout, PlayoutHealth, Schedule};
use crate::ring::{CHANNELS, Ring};
use crate::sinks::SourceTiming;

pub struct AirPlayOutput {
    stream: airplay::Stream,
    receiver: String,
    health: Arc<PlayoutHealth>,
    meter: Arc<FlowMeter>,
    cycles: Arc<Cycles>,
    reported_glitches: [u32; 4],
    reported_state: Option<FlowState>,
}

impl AirPlayOutput {
    pub fn playout(source: &SourceTiming, delay_ms: u32) -> Result<Playout, String> {
        let delay = f64::from(delay_ms) / 1000.0;
        let playout = Playout::new(
            source,
            f64::from(airplay::SAMPLE_RATE),
            unsafe { AudioGetHostClockFrequency() },
            Some(Schedule {
                delay,
                latency: 0.0,
            }),
        )?;
        let minimum = playout.minimum_delay(airplay::LATENCY.as_secs_f64());
        if delay < minimum {
            return Err(format!(
                "AirPlay requires --delay of at least {:.0} ms for this source",
                (minimum * 1000.0).ceil(),
            ));
        }
        Ok(playout)
    }

    pub fn open(
        browser: &airplay::Browser,
        receiver: airplay::ReceiverId,
        playout: Playout,
        ring: Arc<Ring>,
        volume: Arc<AtomicU32>,
    ) -> Self {
        let health = playout.health();
        let meter = Arc::new(FlowMeter::new());
        let cycles = Arc::new(Cycles::new());
        let mut renderer = Renderer {
            ring,
            playout,
            fader: Fader::new(
                f32::from_bits(volume.load(Ordering::Relaxed)),
                f64::from(airplay::SAMPLE_RATE),
            ),
            volume,
            meter: Arc::clone(&meter),
            cycles: Arc::clone(&cycles),
        };
        let frequency = unsafe { AudioGetHostClockFrequency() };
        let receiver_name = receiver.to_string();
        let stream = airplay::Stream::start(browser, receiver, move |mut render| {
            let before = Instant::now();
            let now = unsafe { AudioGetCurrentHostTime() };
            let after = Instant::now();
            // Codex: these clocks share macOS monotonic time. The midpoint bounds
            // sampling error by half the bracket, plus one rounded host tick.
            let anchor = before + after.duration_since(before) / 2;
            let output = host_ticks(render.play_at, anchor, now, frequency);
            renderer.render(&mut render.channels, now, output);
        });
        Self {
            stream,
            receiver: receiver_name,
            health,
            meter,
            cycles,
            reported_glitches: [0; 4],
            reported_state: None,
        }
    }

    pub fn finish(self) -> airplay::Finishing {
        self.stream.finish()
    }

    pub fn cycles(&self) -> &Cycles {
        &self.cycles
    }

    pub fn status(&self) -> FlowStatus {
        flow_status(&self.stream.status(), &self.meter, &self.receiver)
    }

    pub fn log_health(&mut self, route_id: u32, ring: &Ring) {
        self.health
            .log(route_id, ring, true, &mut self.reported_glitches);
        let transport = self.stream.status();
        let state = flow_status(&transport, &self.meter, &self.receiver).state;
        if self.reported_state != Some(state) {
            match &transport {
                airplay::Status::Unreachable(reason) | airplay::Status::Lost(reason) => {
                    tracing::warn!(
                        route_id,
                        receiver = self.receiver,
                        ?state,
                        reason,
                        "AirPlay session ended"
                    );
                }
                _ => {}
            }
            self.reported_state = Some(state);
        }
        if let airplay::Status::Streaming(counters) = transport {
            tracing::debug!(
                route_id,
                ?state,
                bytes = self.meter.bytes(),
                transport_bytes = counters.bytes_sent,
                packets_sent = counters.packets_sent,
                packets_retransmitted = counters.packets_retransmitted,
                "route AirPlay health"
            );
        }
    }
}

struct Renderer {
    ring: Arc<Ring>,
    playout: Playout,
    volume: Arc<AtomicU32>,
    fader: Fader,
    meter: Arc<FlowMeter>,
    cycles: Arc<Cycles>,
}

impl Renderer {
    fn render(&mut self, out: &mut [&mut [f32]; CHANNELS], now: u64, output: u64) {
        self.playout.render(&self.ring, out, now, output);
        if self.playout.is_playing() {
            self.meter.record(
                (out[0].len() * CHANNELS * std::mem::size_of::<f32>()) as u64,
                now,
            );
        }
        self.fader
            .planar(out, f32::from_bits(self.volume.load(Ordering::Relaxed)));
        self.cycles.tick(now);
    }
}

fn host_ticks(play_at: Instant, anchor: Instant, now: u64, frequency: f64) -> u64 {
    if let Some(ahead) = play_at.checked_duration_since(anchor) {
        now.saturating_add((ahead.as_secs_f64() * frequency).round() as u64)
    } else {
        now.saturating_sub(
            (anchor.duration_since(play_at).as_secs_f64() * frequency).round() as u64,
        )
    }
}

fn flow_status(transport: &airplay::Status, meter: &FlowMeter, receiver: &str) -> FlowStatus {
    let mut status = meter.status(Link::Connected {
        peer: Some(receiver.to_string()),
        packets_lost: 0,
        packets_retransmitted: match transport {
            airplay::Status::Streaming(counters) => counters.packets_retransmitted,
            _ => 0,
        },
    });
    match transport {
        airplay::Status::Connecting => status.state = FlowState::Connecting,
        airplay::Status::Unreachable(_) => status.state = FlowState::Unreachable,
        airplay::Status::Lost(_) => status.state = FlowState::Lost,
        airplay::Status::Streaming(_) => {}
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::DeliverySpan;
    use crate::mixer::IDLE;
    use std::time::Duration;

    #[test]
    fn instant_conversion_preserves_future_and_past_offsets() {
        let anchor = Instant::now();
        let now = (1u64 << 56) + 77;
        for frequency in [24_000_000.0, 1_000_000_000.0] {
            let delta = Duration::from_secs(2);
            let ticks = (2.0 * frequency) as u64;
            assert_eq!(
                host_ticks(anchor + delta, anchor, now, frequency),
                now + ticks
            );
            assert_eq!(
                host_ticks(anchor - delta, anchor, now, frequency),
                now - ticks
            );
        }
    }

    #[test]
    fn transport_and_media_progress_report_separate_states() {
        let meter = FlowMeter::new();
        let streaming = airplay::Status::Streaming(airplay::Counters {
            bytes_sent: 10_000,
            packets_retransmitted: 7,
            ..Default::default()
        });
        let status = || flow_status(&streaming, &meter, "receiver");
        assert_eq!(status().state, FlowState::Connected);
        assert_eq!(status().bytes, 0);
        assert_eq!(status().packets_retransmitted, 7);
        let now = unsafe { AudioGetCurrentHostTime() };
        meter.record(8, now);
        assert_eq!(status().state, FlowState::Flowing);
        let age = (4.0 * unsafe { AudioGetHostClockFrequency() }) as u64;
        meter.record(8, now - age);
        assert_eq!(status().state, FlowState::Stalled);
        for (transport, expected) in [
            (airplay::Status::Connecting, FlowState::Connecting),
            (
                airplay::Status::Unreachable("refused".into()),
                FlowState::Unreachable,
            ),
            (airplay::Status::Lost("closed".into()), FlowState::Lost),
        ] {
            assert_eq!(flow_status(&transport, &meter, "receiver").state, expected);
        }
    }

    #[test]
    fn scheduled_silence_does_not_count_as_flow_and_rendered_audio_fades() {
        let timing = SourceTiming {
            sample_rate: 44100.0,
            delivery: Arc::new(DeliverySpan::new(352.0)),
        };
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let mut renderer = Renderer {
            ring: Arc::new(Ring::new()),
            playout: AirPlayOutput::playout(&timing, 2500).unwrap(),
            volume: Arc::clone(&volume),
            fader: Fader::new(1.0, 44100.0),
            meter: Arc::new(FlowMeter::new()),
            cycles: Arc::new(Cycles::new()),
        };
        let frequency = unsafe { AudioGetHostClockFrequency() };
        let start = unsafe { AudioGetCurrentHostTime() };
        let (mut left, mut right) = ([1.0; 352], [1.0; 352]);
        renderer.render(
            &mut [&mut left, &mut right],
            start,
            start + (2.0 * frequency) as u64,
        );
        assert_eq!(renderer.meter.bytes(), 0);
        assert!(left.iter().all(|s| *s == 0.0));
        assert!(renderer.cycles.running(IDLE));
        let captured = start - (0.5 * frequency) as u64;
        renderer
            .ring
            .push_with(44100, start, Some(captured), |_, _| 0.5);
        for i in 0..30 {
            let now = start + (i as f64 * 352.0 / 44100.0 * frequency) as u64;
            if i == 15 {
                volume.store(0.0f32.to_bits(), Ordering::Relaxed);
            }
            renderer.render(
                &mut [&mut left, &mut right],
                now,
                now + (2.0 * frequency) as u64,
            );
            assert_eq!(left, right);
            if i == 14 {
                assert!(left.iter().all(|s| (*s - 0.5).abs() < 0.01));
            }
        }
        assert!(renderer.meter.bytes() > 0);
        assert!(left.iter().all(|s| *s == 0.0));
    }
}
