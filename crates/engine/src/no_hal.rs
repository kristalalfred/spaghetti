//! Claude: what the router names of the Core Audio HAL where there is none,
//! as on iOS: process taps, device buses and device outputs. Every type here
//! is uninhabited, so their constructors are the only code that runs, and a
//! source or route that needs one is refused when it is created.

pub mod audio {
    use std::convert::Infallible;

    pub type DeviceId = Infallible;

    pub fn find_device_by_uid(_uid: &str) -> Option<DeviceId> {
        None
    }
}

pub mod tap {
    use std::convert::Infallible;
    use std::sync::Arc;

    use spaghetti_common::ipc_protocol::{FlowStatus, TapMode};

    use crate::ring::Ring;
    use crate::sinks::SourceTiming;

    pub struct TapSource {
        pub label: String,
        never: Infallible,
    }

    impl TapSource {
        pub fn create(_source_id: u32, _pids: &[u32], _mode: TapMode) -> Result<Self, String> {
            Err("process taps need macOS".into())
        }

        pub fn status(&self) -> FlowStatus {
            match self.never {}
        }

        pub fn set_processes(&mut self, _pids: &[u32]) -> Result<(), String> {
            match self.never {}
        }

        pub fn timing(&mut self) -> Result<SourceTiming, String> {
            match self.never {}
        }

        pub fn add_sink(&mut self, _ring: Arc<Ring>) -> Result<usize, String> {
            match self.never {}
        }

        pub fn remove_sink(&mut self, _slot: usize) {
            match self.never {}
        }
    }
}

pub mod bus {
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    use spaghetti_common::ipc_protocol::TapMode;

    use crate::flow::FlowMeter;
    use crate::mixer::Cycles;

    pub struct DeviceBus(Infallible);

    impl DeviceBus {
        pub fn open(_device_uid: &str) -> Result<Self, String> {
            Err("device buses need macOS".into())
        }

        pub fn attach(
            &mut self,
            _pids: &[u32],
            _mode: TapMode,
            _volume: Arc<AtomicU32>,
            _meter: Arc<FlowMeter>,
            _cycles: Arc<Cycles>,
        ) -> Result<usize, String> {
            match self.0 {}
        }

        pub fn retarget(&self, _slot: usize, _pids: &[u32], _mode: TapMode) -> Result<(), String> {
            match self.0 {}
        }

        pub fn detach(&mut self, _slot: usize) {
            match self.0 {}
        }

        pub fn is_empty(&self) -> bool {
            match self.0 {}
        }
    }
}

pub mod device_output {
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    use spaghetti_common::ipc_protocol::FlowStatus;

    use super::audio::DeviceId;
    use crate::mixer::Cycles;
    use crate::ring::Ring;
    use crate::sinks::SourceTiming;
    use crate::spec::Playback;

    pub struct DeviceOutput(Infallible);

    impl DeviceOutput {
        /// # Safety
        ///
        /// Claude: no device exists to pass, so this never runs.
        pub unsafe fn open(
            device: DeviceId,
            _source: SourceTiming,
            _ring: Arc<Ring>,
            _volume: Arc<AtomicU32>,
            _delay: Option<f64>,
        ) -> Result<Self, String> {
            match device {}
        }

        pub fn status(&self) -> FlowStatus {
            match self.0 {}
        }

        pub fn start(&self) -> Result<(), String> {
            match self.0 {}
        }

        pub fn cycles(&self) -> &Cycles {
            match self.0 {}
        }

        pub fn playback(&self) -> Playback {
            match self.0 {}
        }

        pub fn log_health(&mut self, _route_id: u32, _ring: &Ring) {
            match self.0 {}
        }
    }
}
