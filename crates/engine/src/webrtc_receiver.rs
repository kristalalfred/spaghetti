use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use coreaudio_sys::{AudioGetCurrentHostTime, AudioGetHostClockFrequency};
use spaghetti_common::ipc_protocol::{FlowStatus, WebRtcPeer};

use crate::clock::SenderClock;
use crate::codec::{Codec, DECODED_RATE, Decoder, OPUS_DELAY_FRAMES, StreamFormat};
use crate::delivery::{DeliveryMeter, DeliverySpan};
use crate::ring::{CHANNELS, Ring};
use crate::sinks::{MAX_SINKS, Sinks, SourceTiming};
use crate::webrtc::{IncomingPacket, Media, PeerLink, REORDER_TIMEOUT};

const PACKET_FRAMES: u64 = 960;
const PACKET_SECONDS: f64 = PACKET_FRAMES as f64 / DECODED_RATE as f64;
const ARRIVAL_JITTER: f64 = 0.04;
/// Claude: after a loss str0m holds the next packet up to its reorder timeout,
/// then releases it with the one behind it, a burst of one more packet. Real
/// arrival jitter comes on top.
const DELIVERY_JITTER: f64 = REORDER_TIMEOUT.as_secs_f64() + PACKET_SECONDS + ARRIVAL_JITTER;
const QUEUED_PACKETS: usize = 64;

/// Claude: a source fed by Opus from a WebRTC peer link.
pub struct WebRtcSource {
    link: PeerLink,
    sinks: Arc<Sinks>,
    delivery: Arc<DeliverySpan>,
    concealed: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    decoder: Option<std::thread::JoinHandle<()>>,
    pub label: String,
}

impl WebRtcSource {
    pub fn open(peer: &WebRtcPeer) -> Result<Self, String> {
        let decoder = Decoder::new(
            Codec::Opus,
            StreamFormat {
                sample_rate: DECODED_RATE,
                channels: CHANNELS as u8,
            },
        )?;
        let (packets, received) = std::sync::mpsc::sync_channel(QUEUED_PACKETS);
        let clock = Arc::new(SenderClock::default());
        let media = Media::Receive {
            packets,
            clock: Arc::clone(&clock),
        };
        let link = PeerLink::open(peer, media)?;
        let sinks = Arc::new(Sinks::new());
        let meter = DeliveryMeter::new(f64::from(DECODED_RATE), DELIVERY_JITTER);
        let delivery = meter.span();
        let concealed = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let decoder = std::thread::Builder::new()
            .name("webrtc-decode".into())
            .spawn({
                let sinks = Arc::clone(&sinks);
                let concealed = Arc::clone(&concealed);
                let stop = Arc::clone(&stop);
                move || decode(decoder, received, meter, &clock, &sinks, &concealed, &stop)
            })
            .map_err(|e| format!("start WebRTC decoder: {e}"))?;
        Ok(Self {
            link,
            sinks,
            delivery,
            concealed,
            stop,
            decoder: Some(decoder),
            label: peer.link.clone(),
        })
    }

    pub fn peer(&self) -> &PeerLink {
        &self.link
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
        FlowStatus {
            packets_lost: self.concealed.load(Ordering::Relaxed),
            ..self.link.status()
        }
    }
}

impl Drop for WebRtcSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(decoder) = self.decoder.take() {
            let _ = decoder.join();
        }
    }
}

/// Claude: the decoder thread is the only reader of `sinks`. Packets come in
/// RTP time order with gaps where some never arrived; each missing 20 ms is
/// concealed so the rings stay continuous, and a packet at or behind the last
/// one decoded or concealed is dropped. Concealment bridges gaps the routes'
/// jitter buffers outlast; after a longer one they have re-primed and would
/// only discard it. Each 20 ms lands as a push of its own, so a concealed gap
/// reads as the late delivery it is, not as one long push. A packet's frames
/// end where its RTP span ends, since the decoder drops part of the pre-skip
/// from the first one. Once the sender's clock is known, each push carries
/// when the sender captured its audio.
fn decode(
    mut decoder: Decoder,
    received: Receiver<IncomingPacket>,
    mut meter: DeliveryMeter,
    clock: &SenderClock,
    sinks: &Sinks,
    concealed: &AtomicU64,
    stop: &AtomicBool,
) {
    let ticks_per_second = unsafe { AudioGetHostClockFrequency() };
    let rate = f64::from(DECODED_RATE);
    let mut pcm = Vec::new();
    let mut last: Option<u64> = None;
    while !stop.load(Ordering::Relaxed) {
        let packet = match received.recv_timeout(Duration::from_millis(100)) {
            Ok(packet) => packet,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let missing = match last {
            Some(last) if packet.rtp_time <= last => continue,
            Some(last) => ((packet.rtp_time - last) / PACKET_FRAMES).saturating_sub(1),
            None => 0,
        };
        pcm.clear();
        let bridged = if meter.gap_before(packet.rtp_time as f64 / rate).is_some() {
            missing
        } else {
            0
        };
        for _ in 0..bridged {
            conceal(&mut decoder, &mut pcm);
        }
        concealed.fetch_add(bridged, Ordering::Relaxed);
        if let Err(e) = decoder.decode(&packet.data, &mut pcm) {
            tracing::debug!(error = e, "concealed an undecodable Opus packet");
            conceal(&mut decoder, &mut pcm);
        }
        last = Some(packet.rtp_time);
        let arrival = unsafe { AudioGetCurrentHostTime() } as f64 / ticks_per_second;
        let decoded = (pcm.len() / CHANNELS) as u64;
        let first = (packet.rtp_time + PACKET_FRAMES).wrapping_sub(decoded);
        for (slot, slot_pcm) in pcm.chunks(PACKET_FRAMES as usize * CHANNELS).enumerate() {
            let frames = slot_pcm.len() / CHANNELS;
            let rtp_time = first + slot as u64 * PACKET_FRAMES;
            let start = rtp_time as f64 / rate;
            let due = meter.record(start..start + frames as f64 / rate, arrival);
            let stamp = (due * ticks_per_second) as u64;
            let captured =
                clock.captured(rtp_time.wrapping_sub(u64::from(OPUS_DELAY_FRAMES)), rate);
            unsafe {
                sinks.for_each(|ring| {
                    ring.push_with(frames, stamp, captured, |f, ch| slot_pcm[f * CHANNELS + ch]);
                });
            }
        }
    }
}

fn conceal(decoder: &mut Decoder, pcm: &mut Vec<f32>) {
    if let Err(e) = decoder.decode(&[], pcm) {
        tracing::debug!(error = e, "Opus concealment failed");
    }
}
