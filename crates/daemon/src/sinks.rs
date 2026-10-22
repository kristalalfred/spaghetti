use std::sync::Arc;

use crate::ring::Ring;
use crate::rt_slots::RtSlots;

pub const MAX_SINKS: usize = 8;

/// Claude: the rings of the routes one source feeds. The source's producer
/// thread is the only reader.
pub type Sinks = RtSlots<Arc<Ring>, MAX_SINKS>;
