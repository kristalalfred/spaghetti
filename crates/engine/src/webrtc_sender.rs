use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use spaghetti_common::ipc_protocol::{FlowStatus, WebRtcPeer};
use tokio::sync::mpsc::Sender;

use crate::codec::{Codec, Encoder, OPUS_DELAY_FRAMES};
use crate::host_time;
use crate::mixer::Fader;
use crate::ring::{CHANNELS, Ring};
use crate::sinks::SourceTiming;
use crate::stream_clock::{Crossing, StreamClock};
use crate::webrtc::{Media, OutgoingPacket, PeerLink};

const QUEUED_PACKETS: usize = 64;
const POLL: Duration = Duration::from_millis(5);

/// Claude: a route's output as Opus over a WebRTC peer link. The encoder thread
/// drains the route's ring at the source's own pace.
pub struct WebRtcOutput {
    link: PeerLink,
    stop: Arc<AtomicBool>,
    encoder: Option<std::thread::JoinHandle<()>>,
    reported_overflows: u32,
}

impl WebRtcOutput {
    pub fn open(
        peer: &WebRtcPeer,
        source: SourceTiming,
        ring: Arc<Ring>,
        volume: Arc<AtomicU32>,
    ) -> Result<Self, String> {
        let encoder = Encoder::new(Codec::Opus, source.sample_rate.round() as u32)?;
        let (packets, queued) = tokio::sync::mpsc::channel(QUEUED_PACKETS);
        let link = PeerLink::open(peer, Media::Send(queued))?;
        let stop = Arc::new(AtomicBool::new(false));
        let encoder = std::thread::Builder::new()
            .name("webrtc-encode".into())
            .spawn({
                let stop = Arc::clone(&stop);
                move || encode(encoder, source.sample_rate, &ring, &volume, &packets, &stop)
            })
            .map_err(|e| format!("start WebRTC encoder: {e}"))?;
        Ok(Self {
            link,
            stop,
            encoder: Some(encoder),
            reported_overflows: 0,
        })
    }

    pub fn peer(&self) -> &PeerLink {
        &self.link
    }

    pub fn status(&self) -> FlowStatus {
        self.link.status()
    }

    pub fn log_health(&mut self, route_id: u32, ring: &Ring) {
        let status = self.status();
        tracing::debug!(route_id, state = ?status.state, bytes = status.bytes, peer = status.peer, "route WebRTC health");
        let overflows = ring.overflows();
        if overflows != self.reported_overflows {
            tracing::info!(
                route_id,
                overflows = overflows - self.reported_overflows,
                "route WebRTC encoder fell behind since the last report"
            );
            self.reported_overflows = overflows;
        }
    }
}

impl Drop for WebRtcOutput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(encoder) = self.encoder.take() {
            let _ = encoder.join();
        }
    }
}

/// Claude: each packet the encoder emits advances the RTP time by its 960
/// frames at 48 kHz, so the RTP clock counts audio, whatever rate the source
/// runs at. A packet carries the capture time of the pop that completed it,
/// and, when the source dates its frames, when its own first frame was
/// captured: by the latest pop's date, the stream's first frame was captured
/// `fed` source frames earlier, and the packet's lies its RTP time after that.
/// Across a discontinuity in the ring the RTP time skips what never came, so
/// it stays in step with capture time and the receiver conceals the gap.
fn encode(
    mut encoder: Encoder,
    source_rate: f64,
    ring: &Ring,
    volume: &AtomicU32,
    packets: &Sender<OutgoingPacket>,
    stop: &AtomicBool,
) {
    let frames_per_packet = encoder.frames_per_packet();
    let ticks_per_second = host_time::ticks_per_second();
    let ticks_per_frame = ticks_per_second / source_rate;
    let rtp_rate = f64::from(encoder.format().sample_rate);
    let mut clock = StreamClock::new(
        source_rate,
        rtp_rate,
        frames_per_packet,
        OPUS_DELAY_FRAMES as usize,
    );
    let mut pcm = vec![0.0; frames_per_packet * CHANNELS];
    let mut silence = Vec::new();
    let mut fader = Fader::new(f32::from_bits(volume.load(Ordering::Relaxed)), source_rate);
    let mut rtp_time = 0u64;
    let mut fed = 0u64;
    let mut stream_start: Option<f64> = None;
    let mut overlap = 0;
    while !stop.load(Ordering::Relaxed) {
        if packets.is_closed() {
            ring.take_discontinuity();
            ring.discard(ring.available());
        }
        if let Some(start) = ring.take_discontinuity() {
            match clock.cross(start) {
                Crossing::Continuous => {}
                Crossing::Overlap(frames) => overlap = frames,
                Crossing::Gap {
                    silence: frames,
                    packets: skipped,
                } => {
                    let captured = ring.captured_at(source_rate, ticks_per_second);
                    let captured = captured
                        .checked_sub(Duration::from_secs_f64(frames as f64 / source_rate))
                        .unwrap_or(captured);
                    silence.resize(frames * CHANNELS, 0.0);
                    fed += frames as u64;
                    let encoded = encoder.encode(&silence, |packet| {
                        let _ = packets.try_send(OutgoingPacket {
                            rtp_time,
                            captured,
                            captured_nanos: stream_start
                                .map(|start| (start + rtp_time as f64 * 1e9 / rtp_rate) as u64),
                            data: packet.to_vec(),
                        });
                        rtp_time += frames_per_packet as u64;
                    });
                    if let Err(e) = encoded {
                        tracing::warn!(error = e, "Opus encode failed");
                    }
                    rtp_time += (skipped * frames_per_packet) as u64;
                    fed += clock.packet_frames(skipped);
                }
            }
        }
        overlap -= ring.discard(overlap);
        while overlap == 0 && ring.available() >= frames_per_packet && !packets.is_closed() {
            let captured = ring.captured_at(source_rate, ticks_per_second);
            stream_start = ring.tail_captured(ticks_per_frame).map(|ticks| {
                let nanos = host_time::to_nanos(ticks);
                nanos as f64 - fed as f64 * 1e9 / source_rate
            });
            clock.popping(ring.tail_time(), frames_per_packet);
            ring.pop_into(&mut pcm);
            fed += frames_per_packet as u64;
            let gain = f32::from_bits(volume.load(Ordering::Relaxed));
            fader.interleaved(&mut pcm, CHANNELS, gain);
            let encoded = encoder.encode(&pcm, |packet| {
                let captured_nanos =
                    stream_start.map(|start| (start + rtp_time as f64 * 1e9 / rtp_rate) as u64);
                let _ = packets.try_send(OutgoingPacket {
                    rtp_time,
                    captured,
                    captured_nanos,
                    data: packet.to_vec(),
                });
                rtp_time += frames_per_packet as u64;
            });
            if let Err(e) = encoded {
                tracing::warn!(error = e, "Opus encode failed");
            }
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::ring::Captured;

    const RATE: f64 = 48000.0;

    /// Claude: runs the encoder while `produce` fills its ring, until nothing
    /// more comes out, and returns the packets it sent. `produce` may settle
    /// the encoder: wait until it has sent all it can of what was pushed.
    fn encoded(
        ring: Arc<Ring>,
        produce: impl FnOnce(&Ring, &mut dyn FnMut()),
    ) -> Vec<OutgoingPacket> {
        let (packets, mut sent) = tokio::sync::mpsc::channel(100_000);
        let stop = Arc::new(AtomicBool::new(false));
        let encoder = std::thread::spawn({
            let (ring, stop) = (Arc::clone(&ring), Arc::clone(&stop));
            move || {
                let encoder = Encoder::new(Codec::Opus, RATE as u32).unwrap();
                let volume = AtomicU32::new(1.0f32.to_bits());
                encode(encoder, RATE, &ring, &volume, &packets, &stop);
            }
        });
        let mut out = Vec::new();
        let mut settle = || {
            let mut quiet = Instant::now() + Duration::from_secs(10);
            while Instant::now() < quiet {
                match sent.try_recv() {
                    Ok(packet) => {
                        out.push(packet);
                        quiet = Instant::now() + Duration::from_millis(300);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        };
        produce(&ring, &mut settle);
        settle();
        stop.store(true, Ordering::Relaxed);
        encoder.join().unwrap();
        out
    }

    /// Claude: pushes noise captured from `from` to `to` seconds after
    /// `origin`, in 480-frame callbacks, `pace` apart in real time.
    fn push_run(ring: &Ring, origin: u64, from: f64, to: f64, pace: Duration) {
        let ticks_per_second = host_time::ticks_per_second();
        let mut frame = (from * RATE) as usize;
        while frame < (to * RATE) as usize {
            let captured = origin + (frame as f64 / RATE * ticks_per_second) as u64;
            let mut seed = frame as u32;
            while !ring.push_with(
                480,
                host_time::now(),
                Captured::Measured(captured),
                |_, _| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 8) as f32 / (1 << 24) as f32 - 0.5
                },
            ) {
                std::thread::sleep(Duration::from_millis(1));
            }
            frame += 480;
            std::thread::sleep(pace);
        }
    }

    /// Claude: every packet's capture time lies its RTP time after the
    /// stream's start, so the RTP time skipped each gap in capture.
    fn assert_rtp_follows_capture(packets: &[OutgoingPacket], origin: u64, last_captured: f64) {
        let origin = host_time::to_nanos(origin) as f64;
        let offsets: Vec<f64> = packets
            .iter()
            .map(|packet| {
                let captured = packet.captured_nanos.expect("a capture time") as f64;
                (captured - origin) / 1e9 - packet.rtp_time as f64 / RATE
            })
            .collect();
        let (low, high) = offsets
            .iter()
            .fold((f64::MAX, f64::MIN), |(low, high), &offset| {
                (low.min(offset), high.max(offset))
            });
        assert!(
            high - low < 0.0001 && low.abs() < 0.0001,
            "capture time runs {low} to {high} s off RTP time"
        );
        let last = packets.last().unwrap().rtp_time as f64 / RATE;
        assert!(
            (last - last_captured).abs() < 0.03,
            "the last packet's RTP time is {last} s, its audio was captured at {last_captured} s"
        );
    }

    #[test]
    fn rtp_time_skips_gaps_in_capture_and_skips_runs_never_sent() {
        let ring = Arc::new(Ring::new(RATE));
        let origin = host_time::now();
        let packets = encoded(ring, |ring, settle| {
            push_run(ring, origin, 0.0, 1.0, Duration::ZERO);
            settle();
            push_run(ring, origin, 1.3, 1.4, Duration::ZERO);
            push_run(ring, origin, 1.6, 2.0, Duration::ZERO);
        });
        assert_rtp_follows_capture(&packets, origin, 2.0 - 0.02);
    }

    /// Claude: a run too short to fill a packet sits between two gaps, so the
    /// encoder crosses twice without popping in between; each gap counts once.
    #[test]
    fn rtp_time_counts_back_to_back_gaps_once_each() {
        let ring = Arc::new(Ring::new(RATE));
        let origin = host_time::now();
        let packets = encoded(ring, |ring, settle| {
            push_run(ring, origin, 0.0, 0.5, Duration::ZERO);
            settle();
            push_run(ring, origin, 0.8, 0.81, Duration::ZERO);
            settle();
            push_run(ring, origin, 1.2, 1.6, Duration::ZERO);
        });
        assert_rtp_follows_capture(&packets, origin, 1.6 - 0.02);
    }

    #[test]
    fn rtp_time_skips_gaps_in_capture_while_the_producer_keeps_pushing() {
        let ring = Arc::new(Ring::new(RATE));
        let origin = host_time::now();
        let packets = encoded(ring, |ring, settle| {
            let pace = Duration::from_micros(500);
            push_run(ring, origin, 0.0, 0.1, Duration::ZERO);
            settle();
            push_run(ring, origin, 0.1, 0.5, pace);
            push_run(ring, origin, 0.8, 1.0, pace);
            push_run(ring, origin, 1.05, 1.5, pace);
        });
        assert_rtp_follows_capture(&packets, origin, 1.5 - 0.02);
    }
}
