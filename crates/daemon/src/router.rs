use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use spaghetti_common::ipc_protocol::{Destination, FlowStatus, RouteInfo, SourceInfo, SourceSpec};

use crate::audio;
use crate::device_output::DeviceOutput;
use crate::ring::Ring;
use crate::sinks::SourceTiming;
use crate::srt_receiver::SrtSource;
use crate::srt_sender::SrtOutput;
use crate::tap::TapSource;

enum Source {
    Tap(TapSource),
    Srt(SrtSource),
}

impl Source {
    fn open(source_id: u32, spec: &SourceSpec) -> Result<Self, String> {
        match spec {
            SourceSpec::Tap { pids, mode } => TapSource::create(source_id, pids, *mode).map(Self::Tap),
            SourceSpec::Srt(socket) => SrtSource::open(socket).map(Self::Srt),
        }
    }

    fn timing(&self) -> SourceTiming {
        match self {
            Self::Tap(tap) => tap.timing(),
            Self::Srt(srt) => srt.timing(),
        }
    }

    fn add_sink(&mut self, ring: Arc<Ring>) -> Result<usize, String> {
        match self {
            Self::Tap(tap) => tap.add_sink(ring),
            Self::Srt(srt) => srt.add_sink(ring),
        }
    }

    fn remove_sink(&mut self, slot: usize) {
        match self {
            Self::Tap(tap) => tap.remove_sink(slot),
            Self::Srt(srt) => srt.remove_sink(slot),
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::Tap(tap) => &tap.label,
            Self::Srt(srt) => &srt.label,
        }
    }

    fn status(&self) -> FlowStatus {
        match self {
            Self::Tap(tap) => tap.status(),
            Self::Srt(srt) => srt.status(),
        }
    }
}

enum Output {
    Device(DeviceOutput),
    Srt(SrtOutput),
}

impl Output {
    fn open(
        destination: &Destination,
        source: SourceTiming,
        ring: Arc<Ring>,
        volume: Arc<AtomicU32>,
    ) -> Result<Self, String> {
        match destination {
            Destination::Device { uid } => {
                let device = audio::find_device_by_uid(uid)
                    .ok_or_else(|| format!("device '{uid}' not found"))?;
                unsafe { DeviceOutput::open(device, source, ring, volume) }.map(Self::Device)
            }
            Destination::Srt(socket) => SrtOutput::open(socket, source, ring, volume).map(Self::Srt),
        }
    }

    fn start(&self) -> Result<(), String> {
        match self {
            Self::Device(device) => device.start(),
            Self::Srt(_) => Ok(()),
        }
    }

    fn status(&self) -> FlowStatus {
        match self {
            Self::Device(device) => device.status(),
            Self::Srt(srt) => srt.status(),
        }
    }

    fn log_health(&mut self, route_id: u32, ring: &Ring) {
        match self {
            Self::Device(device) => device.log_health(route_id, ring),
            Self::Srt(srt) => srt.log_health(route_id, ring),
        }
    }
}

struct SourceEntry {
    spec: SourceSpec,
    tag: Option<String>,
    source: Source,
}

struct Route {
    source_id: u32,
    destination: Destination,
    volume: Arc<AtomicU32>,
    sink_slot: usize,
    ring: Arc<Ring>,
    output: Output,
}

/// Claude: the routing graph. Sources feed routes through their sinks, and a
/// source's routes are destroyed before the source.
pub struct Router {
    /// Claude: declared before `sources` so routes are torn down before their sources on drop.
    routes: HashMap<u32, Route>,
    sources: HashMap<u32, SourceEntry>,
    next_source_id: u32,
    next_route_id: u32,
}

impl Router {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
            sources: HashMap::new(),
            next_source_id: 1,
            next_route_id: 1,
        }
    }

    pub fn create_source(&mut self, spec: SourceSpec, tag: Option<String>) -> Result<u32, String> {
        let id = self.next_source_id;
        let source = Source::open(id, &spec)?;
        self.next_source_id += 1;
        tracing::info!(source_id = id, label = source.label(), ?tag, "created source");
        self.sources.insert(id, SourceEntry { spec, tag, source });
        Ok(id)
    }

    pub fn destroy_source(&mut self, source_id: u32) -> Result<(), String> {
        if !self.sources.contains_key(&source_id) {
            return Err(format!("source {source_id} not found"));
        }
        let route_ids: Vec<u32> = self
            .routes
            .iter()
            .filter(|(_, route)| route.source_id == source_id)
            .map(|(&id, _)| id)
            .collect();
        for route_id in route_ids {
            self.destroy_route(route_id)?;
        }
        self.sources.remove(&source_id);
        tracing::info!(source_id, "destroyed source");
        Ok(())
    }

    pub fn list_sources(&self) -> Vec<SourceInfo> {
        self.sources
            .iter()
            .map(|(&id, entry)| SourceInfo {
                source_id: id,
                spec: entry.spec.clone(),
                tag: entry.tag.clone(),
                label: entry.source.label().to_string(),
                status: entry.source.status(),
            })
            .collect()
    }

    pub fn source_count(&self) -> u32 {
        self.sources.len() as u32
    }

    pub fn create_route(
        &mut self,
        source_id: u32,
        destination: Destination,
        initial_volume: f32,
    ) -> Result<u32, String> {
        let source = &mut self
            .sources
            .get_mut(&source_id)
            .ok_or_else(|| format!("source {source_id} not found"))?
            .source;

        let volume = Arc::new(AtomicU32::new(initial_volume.clamp(0.0, 2.0).to_bits()));
        let ring = Arc::new(Ring::new());
        let output = Output::open(
            &destination,
            source.timing(),
            Arc::clone(&ring),
            Arc::clone(&volume),
        )?;
        let sink_slot = source.add_sink(Arc::clone(&ring))?;
        if let Err(e) = output.start() {
            source.remove_sink(sink_slot);
            return Err(e);
        }

        let id = self.next_route_id;
        self.next_route_id += 1;
        tracing::info!(route_id = id, source_id, ?destination, "created route");
        self.routes.insert(
            id,
            Route {
                source_id,
                destination,
                volume,
                sink_slot,
                ring,
                output,
            },
        );
        Ok(id)
    }

    pub fn destroy_route(&mut self, route_id: u32) -> Result<(), String> {
        let Route {
            source_id,
            sink_slot,
            output,
            ..
        } = self
            .routes
            .remove(&route_id)
            .ok_or_else(|| format!("route {route_id} not found"))?;

        drop(output);
        if let Some(entry) = self.sources.get_mut(&source_id) {
            entry.source.remove_sink(sink_slot);
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
                source_id: r.source_id,
                destination: r.destination.clone(),
                volume: f32::from_bits(r.volume.load(Ordering::Relaxed)),
                status: r.output.status(),
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
            .filter(|(_, r)| matches!(&r.destination, Destination::Device { uid } if uid == device_uid))
            .map(|(&id, _)| id)
            .collect()
    }
}
