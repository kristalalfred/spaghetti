use std::sync::Arc;

use crate::delivery::DeliverySpan;
use crate::ring::Ring;
use crate::rt_slots::RtSlots;

pub const MAX_SINKS: usize = 8;

/// Claude: the rings of the routes one source feeds. The source's producer
/// thread is the only reader.
pub type Sinks = RtSlots<Arc<Ring>, MAX_SINKS>;

/// Claude: how a source feeds its sinks: frames at `sample_rate`, landing up to
/// `delivery` frames after they were captured.
#[derive(Debug, Clone)]
pub struct SourceTiming {
    pub sample_rate: f64,
    pub delivery: Arc<DeliverySpan>,
}
