use std::sync::Arc;

use crate::ring::Ring;
use crate::rt_slots::RtSlots;

pub const MAX_SINKS: usize = 8;

/// Claude: the rings of the routes one source feeds. The source's producer
/// thread is the only reader.
pub type Sinks = RtSlots<Arc<Ring>, MAX_SINKS>;

/// Claude: how a source feeds its sinks: frames at `sample_rate`, pushed up to
/// `delivery_jitter` seconds later than that rate's cadence.
#[derive(Debug, Clone, Copy)]
pub struct SourceTiming {
    pub sample_rate: f64,
    pub delivery_jitter: f64,
}
