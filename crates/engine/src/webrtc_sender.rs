use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use coreaudio_sys::AudioGetHostClockFrequency;
use spaghetti_common::ipc_protocol::{FlowStatus, WebRtcPeer};
use tokio::sync::mpsc::Sender;

use crate::codec::{Codec, Encoder};
use crate::mixer::Fader;
use crate::ring::{CHANNELS, Ring};
use crate::sinks::SourceTiming;
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
/// runs at. A packet carries the capture time of the pop that completed it.
fn encode(
    mut encoder: Encoder,
    source_rate: f64,
    ring: &Ring,
    volume: &AtomicU32,
    packets: &Sender<OutgoingPacket>,
    stop: &AtomicBool,
) {
    let frames_per_packet = encoder.frames_per_packet();
    let ticks_per_second = unsafe { AudioGetHostClockFrequency() };
    let mut pcm = vec![0.0; frames_per_packet * CHANNELS];
    let mut fader = Fader::new(f32::from_bits(volume.load(Ordering::Relaxed)));
    let mut rtp_time = 0u64;
    while !stop.load(Ordering::Relaxed) {
        if packets.is_closed() {
            ring.discard(ring.available());
        }
        while ring.available() >= frames_per_packet && !packets.is_closed() {
            let captured = ring.captured_at(source_rate, ticks_per_second);
            ring.pop_into(&mut pcm);
            let gain = f32::from_bits(volume.load(Ordering::Relaxed));
            fader.interleaved(&mut pcm, CHANNELS, gain);
            let encoded = encoder.encode(&pcm, |packet| {
                let _ = packets.try_send(OutgoingPacket {
                    rtp_time,
                    captured,
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
