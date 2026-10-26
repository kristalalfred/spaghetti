use std::sync::Arc;

use spaghetti_common::ipc_protocol::FlowStatus;

use crate::delivery::DeliverySpan;
use crate::flow::{FlowMeter, Link};
use crate::ring::Ring;
use crate::sinks::{Sinks, SourceTiming};

/// Claude: a source whose frames a test pushes itself, from one thread, at
/// `sample_rate` and on time.
pub struct FixtureSource {
    sinks: Arc<Sinks>,
    sample_rate: f64,
}

impl FixtureSource {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            sinks: Arc::new(Sinks::new()),
            sample_rate,
        }
    }

    pub fn sinks(&self) -> Arc<Sinks> {
        Arc::clone(&self.sinks)
    }

    pub fn timing(&self) -> SourceTiming {
        SourceTiming {
            sample_rate: self.sample_rate,
            delivery: Arc::new(DeliverySpan::new(0.0)),
        }
    }

    pub fn add_sink(&mut self, ring: Arc<Ring>) -> Result<usize, String> {
        self.sinks
            .insert(Box::new(ring))
            .map_err(|_| "no free sink".to_string())
    }

    pub fn remove_sink(&mut self, slot: usize) {
        drop(self.sinks.remove(slot));
    }

    pub fn status(&self) -> FlowStatus {
        FlowMeter::new().status(Link::Idle)
    }
}
