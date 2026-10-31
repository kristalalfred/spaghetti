use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use spaghetti_common::ipc_protocol::{FlowStatus, SrtSocket};

use crate::adts;
use crate::codec::{Codec, Encoder};
use crate::host_time;
use crate::mixer::Fader;
use crate::ring::{CHANNELS, Ring};
use crate::sinks::SourceTiming;
use crate::srt::{SrtEndpoint, SrtLink};
use crate::stream_clock::{Crossing, StreamClock};
use crate::ts::{PACKET_LEN, TsMuxer};

/// Claude: SRT live mode carries at most 1316 payload bytes, seven TS packets.
const PACKETS_PER_MESSAGE: usize = 7;
const QUEUED_MESSAGES: usize = 64;
const POLL: Duration = Duration::from_millis(5);
/// Claude: the frames AudioToolbox's AAC encoder primes with, after Apple's
/// TN2258, which its output trails its input by.
const AAC_DELAY_FRAMES: usize = 2112;

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
        let encoder = Encoder::new(Codec::Aac, source.sample_rate.round() as u32)?;
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

/// Claude: across a discontinuity in the ring the stream's PTS skips what
/// never came, so the receiver fills the gap with silence or starts over.
fn encode(
    encoder: Encoder,
    source_rate: f64,
    ring: &Ring,
    volume: &AtomicU32,
    messages: &tokio::sync::mpsc::Sender<(Instant, Bytes)>,
    stop: &AtomicBool,
) {
    let frames_per_packet = encoder.frames_per_packet();
    let ticks_per_second = host_time::ticks_per_second();
    let mut clock = StreamClock::new(
        source_rate,
        f64::from(encoder.format().sample_rate),
        frames_per_packet,
        AAC_DELAY_FRAMES,
    );
    let mut stream = Stream::new(encoder);
    let mut pcm = vec![0.0; frames_per_packet * CHANNELS];
    let mut silence = Vec::new();
    let mut fader = Fader::new(f32::from_bits(volume.load(Ordering::Relaxed)), source_rate);
    let mut overlap = 0;
    let mut closed = false;
    while !stop.load(Ordering::Relaxed) && !closed {
        if let Some(start) = ring.take_discontinuity() {
            match clock.cross(start) {
                Crossing::Continuous => {}
                Crossing::Overlap(frames) => overlap = frames,
                Crossing::Gap {
                    silence: frames,
                    packets,
                } => {
                    let origin = ring.captured_at(source_rate, ticks_per_second);
                    let origin = origin
                        .checked_sub(Duration::from_secs_f64(frames as f64 / source_rate))
                        .unwrap_or(origin);
                    silence.resize(frames * CHANNELS, 0.0);
                    closed = stream.send(&silence, origin, messages);
                    stream.units += packets as u64;
                }
            }
        }
        overlap -= ring.discard(overlap);
        while overlap == 0 && ring.available() >= frames_per_packet && !closed {
            let origin = ring.captured_at(source_rate, ticks_per_second);
            clock.popping(ring.tail_time(), frames_per_packet);
            ring.pop_into(&mut pcm);
            let gain = f32::from_bits(volume.load(Ordering::Relaxed));
            fader.interleaved(&mut pcm, CHANNELS, gain);
            closed = stream.send(&pcm, origin, messages);
        }
        std::thread::sleep(POLL);
    }
}

/// Claude: the AAC encoder and the MPEG-TS stream it feeds, one unit's PTS
/// after another.
struct Stream {
    encoder: Encoder,
    muxer: TsMuxer,
    adts: Vec<u8>,
    ts: Vec<u8>,
    units: u64,
}

impl Stream {
    fn new(encoder: Encoder) -> Self {
        Self {
            encoder,
            muxer: TsMuxer::new(),
            adts: Vec::new(),
            ts: Vec::new(),
            units: 0,
        }
    }

    /// Claude: encodes `pcm` and sends the units it completes, stamped with
    /// `origin`. Returns whether the link closed.
    fn send(
        &mut self,
        pcm: &[f32],
        origin: Instant,
        messages: &tokio::sync::mpsc::Sender<(Instant, Bytes)>,
    ) -> bool {
        let format = self.encoder.format();
        let ticks_per_unit = self.encoder.frames_per_packet() as u64 * 90_000;
        let mut closed = false;
        let Self {
            encoder,
            muxer,
            adts,
            ts,
            units,
        } = self;
        let encoded = encoder.encode(pcm, |unit| {
            adts.clear();
            if let Err(e) = adts::adts_frame(format, unit, adts) {
                tracing::warn!(error = e, "AAC frame does not fit ADTS");
                return;
            }
            ts.clear();
            muxer.mux(
                adts,
                *units * ticks_per_unit / u64::from(format.sample_rate),
                ts,
            );
            *units += 1;
            for message in ts.chunks(PACKET_LEN * PACKETS_PER_MESSAGE) {
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
        closed
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::adts::parse_adts;
    use crate::codec::Decoder;
    use crate::ring::Captured;
    use crate::ts::TsDemuxer;

    const RATE: f64 = 48000.0;

    /// Claude: pushes silence with a click at each of `clicks`, captured from
    /// `from` to `to` seconds after `origin`, in 480-frame callbacks.
    fn push_run(ring: &Ring, origin: u64, from: f64, to: f64, clicks: &[f64]) {
        let ticks_per_second = host_time::ticks_per_second();
        let mut frame = (from * RATE) as usize;
        while frame < (to * RATE) as usize {
            let captured = origin + (frame as f64 / RATE * ticks_per_second) as u64;
            let first = frame;
            assert!(ring.push_with(
                480,
                host_time::now(),
                Captured::Measured(captured),
                |f, _| {
                    let at = first + f;
                    if clicks.iter().any(|click| (click * RATE) as usize == at) {
                        0.9
                    } else {
                        0.0
                    }
                }
            ));
            frame += 480;
        }
    }

    /// Claude: the clicks pushed before and after a 500 ms capture gap, decoded
    /// and laid out by the PTS of the units that carry them, lie as far apart
    /// in media time as they were captured, and the PTS only moves forward.
    #[test]
    fn the_pts_skips_a_capture_gap_and_the_audio_after_it_keeps_its_time() {
        let ring = Arc::new(Ring::new(RATE));
        let origin = host_time::now();
        let (messages, mut sent) = tokio::sync::mpsc::channel(100_000);
        let stop = Arc::new(AtomicBool::new(false));
        let encoder = std::thread::spawn({
            let (ring, stop) = (Arc::clone(&ring), Arc::clone(&stop));
            move || {
                let encoder = Encoder::new(Codec::Aac, RATE as u32).unwrap();
                let volume = AtomicU32::new(1.0f32.to_bits());
                encode(encoder, RATE, &ring, &volume, &messages, &stop);
            }
        });
        let mut payloads = Vec::new();
        let mut settle = || {
            let mut quiet = Instant::now() + Duration::from_secs(10);
            while Instant::now() < quiet {
                match sent.try_recv() {
                    Ok((_, payload)) => {
                        payloads.push(payload);
                        quiet = Instant::now() + Duration::from_millis(300);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        };
        push_run(&ring, origin, 0.0, 1.0, &[0.5]);
        settle();
        push_run(&ring, origin, 1.5, 2.2, &[1.7]);
        settle();
        stop.store(true, Ordering::Relaxed);
        encoder.join().unwrap();

        let mut demuxer = TsDemuxer::new();
        let (mut adts, mut pts_frames, mut decoder) = (Vec::new(), Vec::new(), None);
        let mut timeline = vec![0.0f32; (3.0 * RATE) as usize];
        for payload in &payloads {
            demuxer.push(payload, |es, pts| {
                adts.extend_from_slice(es);
                let start = (pts.expect("a PTS on every unit") as f64 * RATE / 90_000.0) as usize;
                pts_frames.push(start);
                let used = parse_adts(&adts, |format, unit| {
                    let decoder =
                        decoder.get_or_insert_with(|| Decoder::new(Codec::Aac, format).unwrap());
                    let mut pcm = Vec::new();
                    decoder.decode(unit, &mut pcm).unwrap();
                    for (offset, frame) in pcm.chunks(CHANNELS).enumerate() {
                        if let Some(slot) = timeline.get_mut(start + offset) {
                            *slot = frame[0];
                        }
                    }
                });
                adts.drain(..used);
            });
        }
        assert!(
            pts_frames.windows(2).all(|pair| pair[1] > pair[0]),
            "the PTS went back"
        );
        let loudest = |from: usize, to: usize| {
            (from..to)
                .max_by(|&a, &b| timeline[a].abs().total_cmp(&timeline[b].abs()))
                .unwrap()
        };
        let first = loudest(0, timeline.len());
        let second = loudest(first + (0.1 * RATE) as usize, timeline.len());
        let apart = (second - first) as f64 / RATE;
        assert!(
            (apart - 1.2).abs() < 0.001,
            "clicks captured 1.2 s apart lie {apart} s apart in media time"
        );
    }
}
