use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use bytes::Bytes;
use coreaudio_sys::{AudioGetCurrentHostTime, AudioGetHostClockFrequency};
use spaghetti_common::ipc_protocol::{FlowStatus, SrtSocket};

use crate::adts;
use crate::codec::{Codec, DECODED_RATE, Decoder, StreamFormat};
use crate::delivery::{DeliveryMeter, DeliverySpan};
use crate::ring::Ring;
use crate::sinks::{MAX_SINKS, Sinks, SourceTiming};
use crate::srt::{SrtEndpoint, SrtLink};
use crate::ts::TsDemuxer;

/// Claude: srt-tokio releases received packets on a coarse timer, measured as
/// up to 13 ms after their scheduled time, so decoded frames land that unevenly.
const DELIVERY_JITTER: f64 = 0.025;
const QUEUED_PACKETS: usize = 512;
const PTS_RATE: f64 = 90_000.0;

/// Claude: a source fed by AAC in MPEG-TS received over SRT.
pub struct SrtSource {
    link: SrtLink,
    sinks: Arc<Sinks>,
    delivery: Arc<DeliverySpan>,
    stop: Arc<AtomicBool>,
    decoder: Option<std::thread::JoinHandle<()>>,
    pub label: String,
}

impl SrtSource {
    pub fn open(spec: &SrtSocket) -> Result<Self, String> {
        let endpoint = SrtEndpoint::parse(spec)?;
        let (payloads, received) = std::sync::mpsc::sync_channel(QUEUED_PACKETS);
        let sinks = Arc::new(Sinks::new());
        let meter = DeliveryMeter::new(f64::from(DECODED_RATE), DELIVERY_JITTER);
        let delivery = meter.span();
        let stop = Arc::new(AtomicBool::new(false));
        let decoder = std::thread::Builder::new()
            .name("srt-decode".into())
            .spawn({
                let sinks = Arc::clone(&sinks);
                let stop = Arc::clone(&stop);
                move || decode(received, meter, &sinks, &stop)
            })
            .map_err(|e| format!("start SRT decoder: {e}"))?;
        let label = endpoint.label();
        tracing::info!(endpoint = label, "opened SRT source");
        Ok(Self {
            link: SrtLink::receive(endpoint, payloads),
            sinks,
            delivery,
            stop,
            decoder: Some(decoder),
            label,
        })
    }

    pub fn timing(&self) -> SourceTiming {
        SourceTiming {
            sample_rate: f64::from(DECODED_RATE),
            delivery: Arc::clone(&self.delivery),
        }
    }

    pub fn add_sink(&mut self, ring: Arc<Ring>) -> Result<usize, String> {
        self.sinks
            .insert(Box::new(ring))
            .map_err(|_| format!("a source can feed at most {MAX_SINKS} routes"))
    }

    pub fn remove_sink(&mut self, slot: usize) {
        drop(self.sinks.remove(slot));
    }

    pub fn status(&self) -> FlowStatus {
        self.link.status()
    }
}

impl Drop for SrtSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(decoder) = self.decoder.take() {
            let _ = decoder.join();
        }
    }
}

/// Claude: the decoder thread is the only reader of `sinks`. An access unit's
/// media time is the PTS of the PES it starts, or follows the unit before it,
/// so units SRT dropped as too late leave a gap in media time. A gap the
/// routes' span outlasts is filled with silence, so the rings stay in step with
/// media time.
fn decode(received: Receiver<Bytes>, mut meter: DeliveryMeter, sinks: &Sinks, stop: &AtomicBool) {
    let ticks_per_second = unsafe { AudioGetHostClockFrequency() };
    let rate = f64::from(DECODED_RATE);
    let mut demuxer = TsDemuxer::new();
    let mut adts = Vec::new();
    let mut media = 0.0;
    let mut format: Option<StreamFormat> = None;
    let mut decoder: Option<Decoder> = None;
    let mut pcm = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let payload = match received.recv_timeout(Duration::from_millis(100)) {
            Ok(payload) => payload,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        demuxer.push(&payload, |es, pts| {
            if let Some(pts) = pts
                && adts.is_empty()
            {
                media = pts as f64 / PTS_RATE;
            }
            adts.extend_from_slice(es);
            let used = adts::parse_adts(&adts, |config, unit| {
                if format != Some(config) {
                    format = Some(config);
                    tracing::info!(
                        sample_rate = config.sample_rate,
                        channels = config.channels,
                        "SRT stream format"
                    );
                    decoder = Decoder::new(Codec::Aac, config)
                        .inspect_err(|e| tracing::warn!(error = e, "cannot decode SRT stream"))
                        .ok();
                }
                let Some(decoder) = &mut decoder else {
                    return;
                };
                pcm.clear();
                let frames = match decoder.decode(unit, &mut pcm) {
                    Ok(frames) => frames,
                    Err(e) => {
                        tracing::debug!(error = e, "dropped an undecodable AAC frame");
                        return;
                    }
                };
                let channels = usize::from(config.channels);
                let arrival = unsafe { AudioGetCurrentHostTime() } as f64 / ticks_per_second;
                if let Some(gap) = meter.gap_before(media).filter(|_| frames > 0) {
                    fill_silence(&mut meter, sinks, gap, frames, arrival, ticks_per_second);
                }
                let end = media + frames as f64 / rate;
                let stamp = (meter.record(media..end, arrival) * ticks_per_second) as u64;
                media = end;
                unsafe {
                    sinks.for_each(|ring| {
                        ring.push_with(frames, stamp, |f, ch| {
                            pcm[f * channels + ch.min(channels - 1)]
                        });
                    });
                }
            });
            adts.drain(..used);
        });
    }
}

/// Claude: pushes silence over `gap` in pieces no longer than a unit, each
/// recorded as the late delivery it is, as WebRTC concealment does.
fn fill_silence(
    meter: &mut DeliveryMeter,
    sinks: &Sinks,
    gap: Range<f64>,
    unit_frames: usize,
    arrival: f64,
    ticks_per_second: f64,
) {
    let rate = f64::from(DECODED_RATE);
    let mut left = ((gap.end - gap.start) * rate).round() as usize;
    let mut start = gap.start;
    while left > 0 {
        let frames = left.min(unit_frames);
        let end = start + frames as f64 / rate;
        let stamp = (meter.record(start..end, arrival) * ticks_per_second) as u64;
        unsafe {
            sinks.for_each(|ring| {
                ring.push_with(frames, stamp, |_, _| 0.0);
            });
        }
        start = end;
        left -= frames;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;

    use spaghetti_common::ipc_protocol::{FlowState, SrtMode};

    use super::*;
    use crate::codec::Encoder;
    use crate::codec::tests::{measure, tone};
    use crate::delivery::SMALLEST_GAP;
    use crate::ring::CHANNELS;
    use crate::srt_sender::SrtOutput;
    use crate::ts::{PACKET_LEN, TsMuxer};

    /// Claude: a tone as `SrtOutput` sends it: AAC units in ADTS and MPEG-TS,
    /// each unit's PTS from its index, cut into SRT messages.
    fn srt_messages(rate: u32, units: usize) -> (StreamFormat, Vec<Vec<u8>>, Vec<Bytes>) {
        let mut encoder = Encoder::new(Codec::Aac, rate).unwrap();
        let format = encoder.format();
        let per_unit = encoder.frames_per_packet();
        let pcm = tone(f64::from(rate), 1000.0, 0.5, units * per_unit);
        let mut muxer = TsMuxer::new();
        let (mut access_units, mut messages) = (Vec::new(), Vec::new());
        for chunk in pcm.chunks(per_unit * CHANNELS) {
            encoder
                .encode(chunk, |unit| {
                    let mut adts = Vec::new();
                    adts::adts_frame(format, unit, &mut adts).unwrap();
                    let pts = access_units.len() as u64 * per_unit as u64 * 90_000
                        / u64::from(format.sample_rate);
                    let mut stream = Vec::new();
                    muxer.mux(&adts, pts, &mut stream);
                    messages.extend(stream.chunks(PACKET_LEN * 7).map(Bytes::copy_from_slice));
                    access_units.push(unit.to_vec());
                })
                .unwrap();
        }
        (format, access_units, messages)
    }

    /// Claude: the decoder resamples 44.1 kHz AAC to 48 kHz. If its output per
    /// unit wandered by a millisecond, the end of one unit would fall short of
    /// the next unit's PTS, and `decode` would fill the step with silence: a
    /// click every few units on a 44.1 kHz source.
    #[test]
    fn a_44_1_khz_stream_decodes_with_no_step_to_fill_after_its_first_unit() {
        let (format, access_units, messages) = srt_messages(44100, 100);
        assert!(access_units.len() >= 95, "{} units", access_units.len());
        let mut decoder = Decoder::new(Codec::Aac, format).unwrap();
        let mut pcm = Vec::new();
        let decoded: Vec<usize> = access_units
            .iter()
            .map(|unit| decoder.decode(unit, &mut pcm).unwrap())
            .collect();
        let rate = f64::from(DECODED_RATE);
        let media = |unit: usize| (unit * 1024 * 90_000 / 44100) as f64 / PTS_RATE;
        let step = |unit: usize| media(unit + 1) - (media(unit) + decoded[unit] as f64 / rate);
        for (unit, frames) in decoded.iter().enumerate().take(decoded.len() - 1).skip(1) {
            assert!(
                step(unit) <= SMALLEST_GAP,
                "unit {unit} decoded to {frames} frames and ends {:.2} ms short of the next",
                step(unit) * 1000.0
            );
        }

        let (payloads, received) = std::sync::mpsc::sync_channel(messages.len());
        for message in messages {
            payloads.send(message).unwrap();
        }
        drop(payloads);
        let sinks = Sinks::new();
        let ring = Arc::new(Ring::new());
        assert!(sinks.insert(Box::new(Arc::clone(&ring))).is_ok());
        let meter = DeliveryMeter::new(rate, DELIVERY_JITTER);
        decode(received, meter, &sinks, &AtomicBool::new(false));
        let filled = ring.available() - decoded.iter().sum::<usize>();
        let first = if step(0) > SMALLEST_GAP {
            (step(0) * rate).round() as usize
        } else {
            0
        };
        assert_eq!(
            filled, first,
            "frames of silence filled, against the first unit's step"
        );
    }

    fn free_port() -> u16 {
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    fn socket(mode: SrtMode) -> SrtSocket {
        SrtSocket {
            mode,
            latency_ms: 120,
            passphrase: Some("spaghetti-loop-test".into()),
            key_size: Some(16),
        }
    }

    /// Claude: feeds `ring` a 1 kHz tone at 48 kHz in real time until `stop`.
    fn produce(ring: Arc<Ring>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let pcm = tone(48000.0, 1000.0, 0.5, 48000);
            let start = Instant::now();
            let mut sent = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let due = (start.elapsed().as_secs_f64() * 48000.0) as usize;
                while sent + 480 <= due {
                    let offset = sent % 48000;
                    ring.push_with(480, unsafe { AudioGetCurrentHostTime() }, |f, ch| {
                        pcm[((offset + f) % 48000) * CHANNELS + ch]
                    });
                    sent += 480;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    }

    fn collect(ring: &Ring, seconds: f64) -> Vec<f32> {
        let mut out = Vec::new();
        let mut buffer = vec![0.0; 4096 * CHANNELS];
        let start = Instant::now();
        while start.elapsed().as_secs_f64() < seconds {
            let frames = ring.pop_into(&mut buffer);
            out.extend_from_slice(&buffer[..frames * CHANNELS]);
            std::thread::sleep(Duration::from_millis(10));
        }
        out
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_tone_crosses_srt_and_the_sender_takes_a_new_caller() {
        let port = free_port();
        let sent = Arc::new(Ring::new());
        let stop = Arc::new(AtomicBool::new(false));
        let producer = produce(Arc::clone(&sent), Arc::clone(&stop));
        let output = SrtOutput::open(
            &socket(SrtMode::Listen { port }),
            SourceTiming {
                sample_rate: 48000.0,
                delivery: Arc::new(DeliverySpan::new(0.0)),
            },
            Arc::clone(&sent),
            Arc::new(AtomicU32::new(1.0f32.to_bits())),
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(output.status().state, FlowState::Connecting);

        for caller in 0..2 {
            let mut source = SrtSource::open(&socket(SrtMode::Call {
                host: "127.0.0.1".into(),
                port,
            }))
            .unwrap();
            let received = Arc::new(Ring::new());
            source.add_sink(Arc::clone(&received)).unwrap();
            let pcm = tokio::task::spawn_blocking(move || collect(&received, 2.0)).await.unwrap();

            assert_eq!(source.status().state, FlowState::Flowing, "caller {caller}");
            assert_eq!(output.status().state, FlowState::Flowing, "caller {caller}");
            assert!(pcm.len() / CHANNELS > 48000, "caller {caller} got {} frames", pcm.len() / CHANNELS);
            let steady = &pcm[pcm.len() / 2..];
            let (hz, rms) = measure(steady, CHANNELS, 48000.0);
            assert!((hz - 1000.0).abs() < 5.0, "caller {caller}: {hz} Hz");
            assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.03, "caller {caller}: rms {rms}");
            drop(source);
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }

        stop.store(true, Ordering::Relaxed);
        producer.join().unwrap();
    }
}
