//! Claude: a capture endpoint as a source. Its stream runs, at the rate the
//! audio engine mixes the endpoint at, while a route takes the frames, and
//! dates each buffer by the performance counter time WASAPI gives its first
//! frame, placed by the endpoint's frame position.

use std::sync::Arc;

use spaghetti_common::ipc_protocol::{FlowState, FlowStatus};
use windows::Win32::Media::Audio::eCapture;

use super::audio;
use super::capture::{Capture, Dating, Opening, Shared};
use super::com;
use crate::flow::Link;
use crate::ring::Ring;
use crate::sinks::SourceTiming;

/// Claude: what a capture endpoint captures. A failure of its stream ends the
/// source for good.
pub struct InputSource {
    device_uid: String,
    shared: Arc<Shared>,
    /// Claude: the rate the source reported its timing by, which capture
    /// starts with and runs by until the last consumer leaves.
    rate: Option<u32>,
    capture: Option<Capture>,
    /// Claude: why capture last failed to start.
    refused: Option<String>,
    pub label: String,
}

fn read_rate(device_uid: &str) -> Result<u32, String> {
    let device =
        com::device(device_uid).map_err(|_| format!("input device '{device_uid}' not found"))?;
    if audio::flow(&device) != Some(eCapture) {
        return Err(format!("device '{device_uid}' has no input"));
    }
    let rate = com::mix_format(&com::activate(&device)?)?.rate;
    if rate == 0 {
        return Err(format!("input device '{device_uid}' reports no rate"));
    }
    Ok(rate)
}

impl InputSource {
    pub fn create(device_uid: &str) -> Result<Self, String> {
        read_rate(device_uid)?;
        let label = com::device(device_uid)
            .ok()
            .and_then(|device| audio::name(&device))
            .unwrap_or_else(|| device_uid.to_string());
        Ok(Self {
            device_uid: device_uid.to_string(),
            shared: Arc::new(Shared::new()),
            rate: None,
            capture: None,
            refused: None,
            label,
        })
    }

    fn rate(&mut self) -> Result<u32, String> {
        if let Some(reason) = self.shared.ended.get() {
            return Err(reason.clone());
        }
        if self.rate.is_none() {
            self.rate = Some(read_rate(&self.device_uid)?);
        }
        Ok(self.rate.expect("read above"))
    }

    pub fn timing(&mut self) -> Result<SourceTiming, String> {
        Ok(SourceTiming {
            sample_rate: f64::from(self.rate()?),
            delivery: Arc::clone(&self.shared.delivery),
        })
    }

    pub fn add_sink(&mut self, ring: Arc<Ring>) -> Result<usize, String> {
        let rate = self.rate()?;
        let slot = self.shared.attach(ring)?;
        if self.capture.is_none() {
            let uid = self.device_uid.clone();
            let started = Capture::start(
                &self.label,
                rate,
                Dating::Counted,
                Arc::clone(&self.shared),
                move || {
                    let client = com::activate(&com::device(&uid)?)?;
                    Ok(Opening {
                        client,
                        loopback: false,
                    })
                },
            );
            match started {
                Ok(capture) => {
                    tracing::info!(label = self.label, "started input capture");
                    self.capture = Some(capture);
                    self.refused = None;
                }
                Err(reason) => {
                    self.shared.detach(slot);
                    self.rate = None;
                    self.refused = Some(reason.clone());
                    return Err(reason);
                }
            }
        }
        Ok(slot)
    }

    pub fn remove_sink(&mut self, slot: usize) {
        self.shared.detach(slot);
        if self.shared.is_empty() {
            if self.capture.take().is_some() {
                tracing::info!(label = self.label, "stopped input capture");
            }
            self.rate = None;
        }
    }

    /// Claude: `Lost` once a failure ended capture; otherwise `Idle` without
    /// consumers and, with them, `Connected` until the device delivers.
    pub fn status(&self) -> FlowStatus {
        if self.shared.ended.get().is_some() {
            let mut status = self.shared.meter.status(Link::Idle);
            status.state = FlowState::Lost;
            return status;
        }
        let link = if self.capture.is_some() {
            Link::Connected {
                peer: None,
                packets_lost: 0,
                packets_retransmitted: 0,
            }
        } else {
            Link::Idle
        };
        self.shared.meter.status(link)
    }

    pub fn failure(&self) -> Option<String> {
        self.shared
            .ended
            .get()
            .cloned()
            .or_else(|| self.refused.clone())
    }
}
