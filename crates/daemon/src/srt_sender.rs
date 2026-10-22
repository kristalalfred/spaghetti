use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use coreaudio_sys::{AudioGetCurrentHostTime, AudioGetHostClockFrequency};
use spaghetti_common::ipc_protocol::{FlowStatus, SrtSocket};

use crate::aac::{self, AacEncoder, FRAMES_PER_PACKET};
use crate::mixer;
use crate::ring::{CHANNELS, Ring};
use crate::sinks::SourceTiming;
use crate::srt::{SrtEndpoint, SrtLink};
use crate::ts::{PACKET_LEN, TsMuxer};

const BITRATE: u32 = 256_000;
/// Claude: SRT live mode carries at most 1316 payload bytes, seven TS packets.
const PACKETS_PER_MESSAGE: usize = 7;
const QUEUED_MESSAGES: usize = 64;
const POLL: Duration = Duration::from_millis(5);

/// Claude: a route's output as AAC in MPEG-TS over SRT. The encoder thread
/// drains the route's ring at the source's own pace.
pub struct SrtOutput {
    link: SrtLink,
    stop: Arc<AtomicBool>,
    encoder: Option<std::thread::JoinHandle<()>>,
    reported_overflows: u32,
}

impl SrtOutput {
    pub fn open(
        spec: &SrtSocket,
        source: SourceTiming,
        ring: Arc<Ring>,
        volume: Arc<AtomicU32>,
    ) -> Result<Self, String> {
        let endpoint = SrtEndpoint::parse(spec)?;
        let encoder = AacEncoder::new(source.sample_rate.round() as u32, BITRATE)?;
        let (messages, queued) = tokio::sync::mpsc::channel(QUEUED_MESSAGES);
        let stop = Arc::new(AtomicBool::new(false));
        let encoder = std::thread::Builder::new()
            .name("srt-encode".into())
            .spawn({
                let stop = Arc::clone(&stop);
                move || encode(encoder, source.sample_rate, &ring, &volume, &messages, &stop)
            })
            .map_err(|e| format!("start SRT encoder: {e}"))?;
        tracing::info!(endpoint = endpoint.label(), source_rate = source.sample_rate, "opened SRT output");
        Ok(Self {
            link: SrtLink::send(endpoint, queued),
            stop,
            encoder: Some(encoder),
            reported_overflows: 0,
        })
    }

    pub fn status(&self) -> FlowStatus {
        self.link.status()
    }

    pub fn log_health(&mut self, route_id: u32, ring: &Ring) {
        let status = self.status();
        tracing::debug!(
            route_id,
            state = ?status.state,
            bytes = status.bytes,
            packets_lost = status.packets_lost,
            packets_retransmitted = status.packets_retransmitted,
            "route SRT health"
        );
        let overflows = ring.overflows();
        if overflows != self.reported_overflows {
            tracing::info!(
                route_id,
                overflows = overflows - self.reported_overflows,
                "route SRT encoder fell behind since the last report"
            );
            self.reported_overflows = overflows;
        }
    }
}

impl Drop for SrtOutput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(encoder) = self.encoder.take() {
            let _ = encoder.join();
        }
    }
}

/// Claude: when the oldest frame in `ring` was captured, from the host time of
/// the latest push and the frames buffered since. Stamping packets with it
/// makes the receiver release them at the source's cadence, not the encoder's.
fn captured_at(ring: &Ring, sample_rate: f64, ticks_per_second: f64) -> Instant {
    let last = ring.last_push();
    let now = Instant::now();
    let since_push = (unsafe { AudioGetCurrentHostTime() } as u32).wrapping_sub(last.at) as f64
        / ticks_per_second;
    let age = since_push + last.buffered as f64 / sample_rate;
    now.checked_sub(Duration::from_secs_f64(age)).unwrap_or(now)
}

fn encode(
    mut encoder: AacEncoder,
    source_rate: f64,
    ring: &Ring,
    volume: &AtomicU32,
    messages: &tokio::sync::mpsc::Sender<(Instant, Bytes)>,
    stop: &AtomicBool,
) {
    let config = encoder.config();
    let ticks_per_second = unsafe { AudioGetHostClockFrequency() };
    let ticks_per_unit = FRAMES_PER_PACKET as u64 * 90_000;
    let mut pcm = vec![0.0; FRAMES_PER_PACKET * CHANNELS];
    let mut muxer = TsMuxer::new();
    let (mut adts, mut stream) = (Vec::new(), Vec::new());
    let mut units = 0u64;
    let mut closed = false;
    while !stop.load(Ordering::Relaxed) && !closed {
        while ring.available() >= FRAMES_PER_PACKET && !closed {
            let origin = captured_at(ring, source_rate, ticks_per_second);
            ring.pop_into(&mut pcm);
            mixer::apply_volume(&mut pcm, f32::from_bits(volume.load(Ordering::Relaxed)));
            let encoded = encoder.encode(&pcm, |unit| {
                adts.clear();
                if let Err(e) = aac::adts_frame(config, unit, &mut adts) {
                    tracing::warn!(error = e, "AAC frame does not fit ADTS");
                    return;
                }
                stream.clear();
                muxer.mux(&adts, units * ticks_per_unit / u64::from(config.sample_rate), &mut stream);
                units += 1;
                for message in stream.chunks(PACKET_LEN * PACKETS_PER_MESSAGE) {
                    if let Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) =
                        messages.try_send((origin, Bytes::copy_from_slice(message)))
                    {
                        closed = true;
                        return;
                    }
                }
            });
            if let Err(e) = encoded {
                tracing::warn!(error = e, "AAC encode failed");
            }
        }
        std::thread::sleep(POLL);
    }
}
