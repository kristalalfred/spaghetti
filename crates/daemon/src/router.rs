use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use coreaudio_sys::AudioObjectID;
use spaghetti_common::ipc_protocol::{RouteInfo, TapInfo, TapMode};

use crate::device_output::DeviceOutput;
use crate::ring::Ring;
use crate::tap::TapSource;

/// A live audio route: reads from a tap's aggregate device, writes to a virtual device.
struct Route {
    tap_id: u32,
    device_uid: String,
    volume: Arc<AtomicU32>,
    sink_slot: usize,
    ring: Arc<Ring>,
    output: DeviceOutput,
}

/// Claude: the routing graph. Sources feed routes through their sinks, and a
/// source's routes are destroyed before the source.
pub struct Router {
    /// Claude: declared before `taps` so routes are torn down before their taps on drop.
    routes: HashMap<u32, Route>,
    taps: HashMap<u32, TapSource>,
    next_tap_id: u32,
    next_route_id: u32,
}

impl Router {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
            taps: HashMap::new(),
            next_tap_id: 1,
            next_route_id: 1,
        }
    }

    pub fn create_tap(&mut self, pid: u32, mode: TapMode) -> Result<u32, String> {
        let id = self.next_tap_id;
        let tap = TapSource::create(id, pid, mode)?;
        self.next_tap_id += 1;
        self.taps.insert(id, tap);
        Ok(id)
    }

    pub fn destroy_tap(&mut self, tap_id: u32) -> Result<(), String> {
        if !self.taps.contains_key(&tap_id) {
            return Err(format!("tap {tap_id} not found"));
        }
        let route_ids: Vec<u32> = self
            .routes
            .iter()
            .filter(|(_, route)| route.tap_id == tap_id)
            .map(|(&id, _)| id)
            .collect();
        for route_id in route_ids {
            self.destroy_route(route_id)?;
        }
        self.taps.remove(&tap_id);
        tracing::info!(tap_id, "destroyed process tap");
        Ok(())
    }

    pub fn list_taps(&self) -> Vec<TapInfo> {
        self.taps
            .iter()
            .map(|(&id, tap)| TapInfo {
                tap_id: id,
                pid: tap.pid,
                app_name: tap.app_name.clone(),
                mode: tap.mode,
            })
            .collect()
    }

    pub fn tap_count(&self) -> u32 {
        self.taps.len() as u32
    }

    pub fn create_route(
        &mut self,
        tap_id: u32,
        dest_device_uid: &str,
        dest_device_id: AudioObjectID,
        initial_volume: f32,
    ) -> Result<u32, String> {
        let tap = self
            .taps
            .get_mut(&tap_id)
            .ok_or_else(|| format!("tap {tap_id} not found"))?;

        let volume = Arc::new(AtomicU32::new(initial_volume.to_bits()));
        let ring = Arc::new(Ring::new());
        let output = unsafe {
            DeviceOutput::open(
                dest_device_id,
                tap.sample_rate(),
                Arc::clone(&ring),
                Arc::clone(&volume),
            )
        }?;
        let sink_slot = tap.add_sink(Arc::clone(&ring))?;
        if let Err(e) = output.start() {
            tap.remove_sink(sink_slot);
            return Err(e);
        }

        let id = self.next_route_id;
        self.next_route_id += 1;
        self.routes.insert(
            id,
            Route {
                tap_id,
                device_uid: dest_device_uid.to_string(),
                volume,
                sink_slot,
                ring,
                output,
            },
        );

        tracing::info!(route_id = id, tap_id, dest = dest_device_uid, "created route");
        Ok(id)
    }

    pub fn destroy_route(&mut self, route_id: u32) -> Result<(), String> {
        let Route {
            tap_id,
            sink_slot,
            output,
            ..
        } = self
            .routes
            .remove(&route_id)
            .ok_or_else(|| format!("route {route_id} not found"))?;

        drop(output);
        if let Some(tap) = self.taps.get_mut(&tap_id) {
            tap.remove_sink(sink_slot);
        }

        tracing::info!(route_id, "destroyed route");
        Ok(())
    }

    pub fn set_volume(&mut self, route_id: u32, volume: f32) -> Result<(), String> {
        let route = self
            .routes
            .get(&route_id)
            .ok_or_else(|| format!("route {route_id} not found"))?;

        let clamped = volume.clamp(0.0, 2.0);
        route.volume.store(clamped.to_bits(), Ordering::Release);
        tracing::debug!(route_id, volume = clamped, "updated volume");
        Ok(())
    }

    pub fn list_routes(&self) -> Vec<RouteInfo> {
        self.routes
            .iter()
            .map(|(&id, r)| RouteInfo {
                route_id: id,
                tap_id: r.tap_id,
                device_uid: r.device_uid.clone(),
                volume: f32::from_bits(r.volume.load(Ordering::Relaxed)),
            })
            .collect()
    }

    pub fn route_count(&self) -> u32 {
        self.routes.len() as u32
    }

    pub fn log_health(&mut self) {
        for (&route_id, route) in &mut self.routes {
            route.output.log_health(route_id, &route.ring);
        }
    }

    /// Return every route ID currently targeting the given device UID.
    pub fn routes_targeting(&self, device_uid: &str) -> Vec<u32> {
        self.routes
            .iter()
            .filter(|(_, r)| r.device_uid == device_uid)
            .map(|(&id, _)| id)
            .collect()
    }
}
