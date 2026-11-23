//! Claude: the device bus of a backend without one: a tap routed to a
//! device always goes through a ring there.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use spaghetti_common::ipc_protocol::TapMode;

use crate::flow::FlowMeter;
use crate::mixer::Cycles;
use crate::spec::Tapped;

pub const DIRECT: bool = false;

pub struct DeviceBus(Infallible);

impl DeviceBus {
    pub fn open(_device_uid: &str) -> Result<Self, String> {
        Err("device buses need Core Audio".into())
    }

    pub fn attach(
        &mut self,
        _tapped: Tapped,
        _mode: TapMode,
        _volume: Arc<AtomicU32>,
        _meter: Arc<FlowMeter>,
        _cycles: Arc<Cycles>,
    ) -> Result<usize, String> {
        match self.0 {}
    }

    pub fn retarget(&self, _slot: usize, _tapped: &Tapped, _mode: TapMode) -> Result<(), String> {
        match self.0 {}
    }

    pub fn detach(&mut self, _slot: usize) {
        match self.0 {}
    }

    pub fn is_empty(&self) -> bool {
        match self.0 {}
    }
}
