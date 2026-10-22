use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use bytes::Bytes;
use coreaudio_sys::AudioGetCurrentHostTime;
use spaghetti_common::ipc_protocol::{FlowStatus, SrtSocket};

use crate::aac::{self, AacConfig, AacDecoder};
use crate::ring::Ring;
use crate::sinks::{MAX_SINKS, Sinks, SourceTiming};
use crate::srt::{SrtEndpoint, SrtLink};
use crate::ts::TsDemuxer;

/// Claude: every received stream is decoded at this rate, so routes can be
/// set up before the first packet says what the stream's rate is.
const OUTPUT_RATE: f64 = 48000.0;
/// Claude: srt-tokio releases received packets on a coarse timer, measured as
/// up to 13 ms after their scheduled time, so decoded frames land that unevenly.
const DELIVERY_JITTER: f64 = 0.025;
const QUEUED_PACKETS: usize = 512;

/// Claude: a source fed by AAC in MPEG-TS received over SRT.
pub struct SrtSource {
    link: SrtLink,
    sinks: Arc<Sinks>,
    stop: Arc<AtomicBool>,
    decoder: Option<std::thread::JoinHandle<()>>,
    pub label: String,
}

impl SrtSource {
    pub fn open(spec: &SrtSocket) -> Result<Self, String> {
        let endpoint = SrtEndpoint::parse(spec)?;
        let (payloads, received) = std::sync::mpsc::sync_channel(QUEUED_PACKETS);
        let sinks = Arc::new(Sinks::new());
        let stop = Arc::new(AtomicBool::new(false));
        let decoder = std::thread::Builder::new()
            .name("srt-decode".into())
            .spawn({
                let sinks = Arc::clone(&sinks);
                let stop = Arc::clone(&stop);
                move || decode(received, &sinks, &stop)
            })
            .map_err(|e| format!("start SRT decoder: {e}"))?;
        let label = endpoint.label();
        tracing::info!(endpoint = label, "opened SRT source");
        Ok(Self {
            link: SrtLink::receive(endpoint, payloads),
            sinks,
            stop,
            decoder: Some(decoder),
            label,
        })
    }

    pub fn timing(&self) -> SourceTiming {
        SourceTiming {
            sample_rate: OUTPUT_RATE,
            delivery_jitter: DELIVERY_JITTER,
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

/// Claude: the decoder thread is the only reader of `sinks`.
fn decode(received: Receiver<Bytes>, sinks: &Sinks, stop: &AtomicBool) {
    let mut demuxer = TsDemuxer::new();
    let mut adts = Vec::new();
    let mut format: Option<AacConfig> = None;
    let mut decoder: Option<AacDecoder> = None;
    let mut pcm = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let payload = match received.recv_timeout(Duration::from_millis(100)) {
            Ok(payload) => payload,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        demuxer.push(&payload, |es, _| adts.extend_from_slice(es));
        let used = aac::parse_adts(&adts, |config, unit| {
            if format != Some(config) {
                format = Some(config);
                tracing::info!(
                    sample_rate = config.sample_rate,
                    channels = config.channels,
                    "SRT stream format"
                );
                decoder = AacDecoder::new(config, OUTPUT_RATE)
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
            let now = unsafe { AudioGetCurrentHostTime() };
            unsafe {
                sinks.for_each(|ring| {
                    ring.push_with(frames, now, |f, ch| pcm[f * channels + ch.min(channels - 1)]);
                });
            }
        });
        adts.drain(..used);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;

    use spaghetti_common::ipc_protocol::{FlowState, SrtMode};

    use super::*;
    use crate::aac::tests::{measure, tone};
    use crate::ring::CHANNELS;
    use crate::srt_sender::SrtOutput;

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
                delivery_jitter: 0.0,
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
