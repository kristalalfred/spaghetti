use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use spaghetti_common::ipc_protocol::{
    Destination, FlowState, FlowStatus, PeerRef, RouteInfo, SourceInfo, SourceSpec,
};

use crate::audio;
use crate::bus::DeviceBus;
use crate::device_output::DeviceOutput;
#[cfg(test)]
use crate::fixture::FixtureSource;
use crate::flow::{FlowMeter, Link};
use crate::mixer::{Cycles, FADE_LIMIT, IDLE, RAMP};
use crate::ring::Ring;
use crate::sinks::SourceTiming;
use crate::srt_receiver::SrtSource;
use crate::srt_sender::SrtOutput;
use crate::tap::TapSource;
use crate::webrtc::PeerLink;
use crate::webrtc_receiver::WebRtcSource;
use crate::webrtc_sender::WebRtcOutput;

enum Source {
    Tap(TapSource),
    Srt(SrtSource),
    WebRtc(WebRtcSource),
    #[cfg(test)]
    Fixture(FixtureSource),
}

impl Source {
    fn open(source_id: u32, spec: &SourceSpec) -> Result<Self, String> {
        match spec {
            SourceSpec::Tap { pids, mode } => TapSource::create(source_id, pids, *mode).map(Self::Tap),
            SourceSpec::Srt(socket) => SrtSource::open(socket).map(Self::Srt),
            SourceSpec::WebRtc(peer) => WebRtcSource::open(peer).map(Self::WebRtc),
        }
    }

    fn timing(&mut self) -> Result<SourceTiming, String> {
        match self {
            Self::Tap(tap) => tap.timing(),
            Self::Srt(srt) => Ok(srt.timing()),
            Self::WebRtc(webrtc) => Ok(webrtc.timing()),
            #[cfg(test)]
            Self::Fixture(fixture) => Ok(fixture.timing()),
        }
    }

    fn add_sink(&mut self, ring: Arc<Ring>) -> Result<usize, String> {
        match self {
            Self::Tap(tap) => tap.add_sink(ring),
            Self::Srt(srt) => srt.add_sink(ring),
            Self::WebRtc(webrtc) => webrtc.add_sink(ring),
            #[cfg(test)]
            Self::Fixture(fixture) => fixture.add_sink(ring),
        }
    }

    fn remove_sink(&mut self, slot: usize) {
        match self {
            Self::Tap(tap) => tap.remove_sink(slot),
            Self::Srt(srt) => srt.remove_sink(slot),
            Self::WebRtc(webrtc) => webrtc.remove_sink(slot),
            #[cfg(test)]
            Self::Fixture(fixture) => fixture.remove_sink(slot),
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::Tap(tap) => &tap.label,
            Self::Srt(srt) => &srt.label,
            Self::WebRtc(webrtc) => &webrtc.label,
            #[cfg(test)]
            Self::Fixture(_) => "fixture",
        }
    }

    fn status(&self) -> FlowStatus {
        match self {
            Self::Tap(tap) => tap.status(),
            Self::Srt(srt) => srt.status(),
            Self::WebRtc(webrtc) => webrtc.status(),
            #[cfg(test)]
            Self::Fixture(fixture) => fixture.status(),
        }
    }

    fn peer(&self) -> Option<&PeerLink> {
        match self {
            Self::WebRtc(webrtc) => Some(webrtc.peer()),
            _ => None,
        }
    }
}

enum Output {
    Device(DeviceOutput),
    Srt(SrtOutput),
    WebRtc(WebRtcOutput),
}

impl Output {
    fn open(
        destination: &Destination,
        source: SourceTiming,
        ring: Arc<Ring>,
        volume: Arc<AtomicU32>,
    ) -> Result<Self, String> {
        match destination {
            Destination::Device { uid, delay_ms } => {
                let device = audio::find_device_by_uid(uid)
                    .ok_or_else(|| format!("device '{uid}' not found"))?;
                let delay = delay_ms.map(|ms| f64::from(ms) / 1000.0);
                unsafe { DeviceOutput::open(device, source, ring, volume, delay) }.map(Self::Device)
            }
            Destination::Srt(socket) => SrtOutput::open(socket, source, ring, volume).map(Self::Srt),
            Destination::WebRtc(peer) => {
                WebRtcOutput::open(peer, source, ring, volume).map(Self::WebRtc)
            }
        }
    }

    fn start(&self) -> Result<(), String> {
        match self {
            Self::Device(device) => device.start(),
            Self::Srt(_) | Self::WebRtc(_) => Ok(()),
        }
    }

    fn status(&self) -> FlowStatus {
        match self {
            Self::Device(device) => device.status(),
            Self::Srt(srt) => srt.status(),
            Self::WebRtc(webrtc) => webrtc.status(),
        }
    }

    fn log_health(&mut self, route_id: u32, ring: &Ring) {
        match self {
            Self::Device(device) => device.log_health(route_id, ring),
            Self::Srt(srt) => srt.log_health(route_id, ring),
            Self::WebRtc(webrtc) => webrtc.log_health(route_id, ring),
        }
    }

    fn peer(&self) -> Option<&PeerLink> {
        match self {
            Self::WebRtc(webrtc) => Some(webrtc.peer()),
            _ => None,
        }
    }

    fn cycles(&self) -> Option<&Cycles> {
        match self {
            Self::Device(device) => Some(device.cycles()),
            Self::Srt(_) | Self::WebRtc(_) => None,
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
    path: Path,
}

enum Path {
    /// Claude: the source feeds the output through a ring in one of its sinks.
    Ring {
        sink_slot: usize,
        ring: Arc<Ring>,
        output: Output,
    },
    /// Claude: the destination device's bus taps the app and mixes it in.
    Bus {
        slot: usize,
        meter: Arc<FlowMeter>,
        cycles: Arc<Cycles>,
    },
}

impl Path {
    fn status(&self) -> FlowStatus {
        match self {
            Self::Ring { output, .. } => output.status(),
            Self::Bus { meter, .. } => meter.status(Link::Connected {
                peer: None,
                packets_lost: 0,
                packets_retransmitted: 0,
            }),
        }
    }

    fn peer(&self) -> Option<&PeerLink> {
        match self {
            Self::Ring { output, .. } => output.peer(),
            Self::Bus { .. } => None,
        }
    }

    /// Claude: the cycles of whatever plays the route on a device.
    fn cycles(&self) -> Option<&Cycles> {
        match self {
            Self::Ring { output, .. } => output.cycles(),
            Self::Bus { cycles, .. } => Some(cycles),
        }
    }
}

/// Claude: the routing graph. Sources feed routes through their sinks, and a
/// source's routes are destroyed before the source. A tap routed to a device
/// skips the sinks and is mixed by that device's bus instead.
pub struct Router {
    /// Claude: declared before `sources` so routes are torn down before their sources on drop.
    routes: HashMap<u32, Route>,
    sources: HashMap<u32, SourceEntry>,
    buses: HashMap<String, DeviceBus>,
    next_source_id: u32,
    next_route_id: u32,
}

impl Router {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
            sources: HashMap::new(),
            buses: HashMap::new(),
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

    pub fn set_tap_processes(&mut self, source_id: u32, pids: Vec<u32>) -> Result<(), String> {
        let entry = self
            .sources
            .get_mut(&source_id)
            .ok_or_else(|| format!("source {source_id} not found"))?;
        let (Source::Tap(tap), SourceSpec::Tap { pids: tapped, .. }) =
            (&mut entry.source, &mut entry.spec)
        else {
            return Err(format!("source {source_id} is not a tap"));
        };
        tap.set_processes(&pids)?;
        *tapped = pids;
        let SourceSpec::Tap { pids, mode } = &entry.spec else {
            unreachable!("matched as a tap above");
        };
        for route in self.routes.values().filter(|route| route.source_id == source_id) {
            if let (Path::Bus { slot, .. }, Destination::Device { uid, .. }) =
                (&route.path, &route.destination)
                && let Some(bus) = self.buses.get(uid)
            {
                bus.retarget(*slot, pids, *mode)?;
            }
        }
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
                status: self.source_status(id, &entry.source),
                local_description: entry.source.peer().and_then(PeerLink::local_description),
            })
            .collect()
    }

    pub fn set_remote_description(
        &self,
        peer: PeerRef,
        generation: u32,
        sdp: String,
    ) -> Result<(), String> {
        let link = match peer {
            PeerRef::Source(id) => self
                .sources
                .get(&id)
                .ok_or_else(|| format!("source {id} not found"))?
                .source
                .peer()
                .ok_or_else(|| format!("source {id} is not a WebRTC peer"))?,
            PeerRef::Route(id) => self
                .routes
                .get(&id)
                .ok_or_else(|| format!("route {id} not found"))?
                .path
                .peer()
                .ok_or_else(|| format!("route {id} is not a WebRTC peer"))?,
        };
        link.set_remote_description(generation, sdp)
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
        let entry = self
            .sources
            .get_mut(&source_id)
            .ok_or_else(|| format!("source {source_id} not found"))?;

        if matches!(
            destination,
            Destination::Device {
                delay_ms: Some(_),
                ..
            }
        ) && matches!(entry.source, Source::Srt(_))
        {
            return Err(
                "an SRT source does not date its frames, so it cannot play on a delay".into(),
            );
        }
        let volume = Arc::new(AtomicU32::new(initial_volume.clamp(0.0, 2.0).to_bits()));
        let path = match (&entry.spec, &destination) {
            (
                SourceSpec::Tap { pids, mode },
                Destination::Device {
                    uid,
                    delay_ms: None,
                },
            ) => {
                let bus = match self.buses.entry(uid.clone()) {
                    Entry::Occupied(bus) => bus.into_mut(),
                    Entry::Vacant(vacant) => vacant.insert(DeviceBus::open(uid)?),
                };
                let meter = Arc::new(FlowMeter::new());
                let cycles = Arc::new(Cycles::new());
                let attached = bus.attach(
                    pids,
                    *mode,
                    Arc::clone(&volume),
                    Arc::clone(&meter),
                    Arc::clone(&cycles),
                );
                match attached {
                    Ok(slot) => Path::Bus {
                        slot,
                        meter,
                        cycles,
                    },
                    Err(e) => {
                        if bus.is_empty() {
                            self.buses.remove(uid);
                        }
                        return Err(e);
                    }
                }
            }
            _ => {
                let source = &mut entry.source;
                let ring = Arc::new(Ring::new());
                let output = Output::open(
                    &destination,
                    source.timing()?,
                    Arc::clone(&ring),
                    Arc::clone(&volume),
                )?;
                let sink_slot = source.add_sink(Arc::clone(&ring))?;
                if let Err(e) = output.start() {
                    source.remove_sink(sink_slot);
                    return Err(e);
                }
                Path::Ring {
                    sink_slot,
                    ring,
                    output,
                }
            }
        };

        let id = self.next_route_id;
        self.next_route_id += 1;
        tracing::info!(route_id = id, source_id, ?destination, "created route");
        self.routes.insert(
            id,
            Route {
                source_id,
                destination,
                volume,
                path,
            },
        );
        Ok(id)
    }

    /// Claude: a route playing on a device fades out over `RAMP` before it
    /// goes, its tap still muting the app meanwhile. The cycle running when the
    /// ramp ends may still be writing it, and two more let the device play out
    /// what it queued before a bus closing with the route discards it.
    pub fn destroy_route(&mut self, route_id: u32) -> Result<(), String> {
        let Route {
            source_id,
            destination,
            path,
            volume,
        } = self
            .routes
            .remove(&route_id)
            .ok_or_else(|| format!("route {route_id} not found"))?;
        if let Some(cycles) = path.cycles().filter(|cycles| cycles.running(IDLE)) {
            volume.store(0f32.to_bits(), Ordering::Release);
            std::thread::sleep(RAMP);
            cycles.wait(3, FADE_LIMIT);
        }

        match (path, destination) {
            (Path::Ring { sink_slot, output, .. }, _) => {
                drop(output);
                if let Some(entry) = self.sources.get_mut(&source_id) {
                    entry.source.remove_sink(sink_slot);
                }
            }
            (Path::Bus { slot, .. }, Destination::Device { uid, .. }) => {
                if let Some(bus) = self.buses.get_mut(&uid) {
                    bus.detach(slot);
                    if bus.is_empty() {
                        self.buses.remove(&uid);
                        tracing::info!(device_uid = uid, "closed device bus");
                    }
                }
            }
            (Path::Bus { .. }, _) => unreachable!("only device routes use a bus"),
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

    /// Claude: sets a route's volume and, while the route plays on a device,
    /// returns once its output has ramped there: the ramp lasts `RAMP`, and
    /// the cycle running when it ends may still be writing it.
    pub fn fade_volume(&mut self, route_id: u32, volume: f32) -> Result<(), String> {
        self.set_volume(route_id, volume)?;
        let cycles = self
            .routes
            .get(&route_id)
            .and_then(|route| route.path.cycles());
        if let Some(cycles) = cycles.filter(|cycles| cycles.running(IDLE)) {
            std::thread::sleep(RAMP);
            cycles.wait(2, FADE_LIMIT);
        }
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
                status: r.path.status(),
                local_description: r.path.peer().and_then(PeerLink::local_description),
            })
            .collect()
    }

    pub fn route_count(&self) -> u32 {
        self.routes.len() as u32
    }

    pub fn log_health(&mut self) {
        for (&route_id, route) in &mut self.routes {
            if let Path::Ring { ring, output, .. } = &mut route.path {
                output.log_health(route_id, ring);
            }
        }
    }

    /// Claude: a tap's own capture says nothing about the routes its device
    /// buses mix, so a tap that only feeds buses reports the busiest of those.
    fn source_status(&self, source_id: u32, source: &Source) -> FlowStatus {
        let status = source.status();
        if !matches!(source, Source::Tap(_)) || status.state != FlowState::Idle {
            return status;
        }
        self.routes
            .values()
            .filter(|route| route.source_id == source_id && matches!(route.path, Path::Bus { .. }))
            .map(|route| route.path.status())
            .max_by_key(|status| status.bytes)
            .unwrap_or(status)
    }

    /// Return every route ID currently targeting the given device UID.
    pub fn routes_targeting(&self, device_uid: &str) -> Vec<u32> {
        self.routes
            .iter()
            .filter(|(_, r)| matches!(&r.destination, Destination::Device { uid, .. } if uid == device_uid))
            .map(|(&id, _)| id)
            .collect()
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl Router {
    /// Claude: a source the test feeds through the returned sinks. No spec
    /// describes it, so it is stored as a tap of no processes.
    pub fn create_fixture_source(&mut self, sample_rate: f64) -> (u32, Arc<crate::sinks::Sinks>) {
        let id = self.next_source_id;
        self.next_source_id += 1;
        let fixture = FixtureSource::new(sample_rate);
        let sinks = fixture.sinks();
        let spec = SourceSpec::Tap {
            pids: Vec::new(),
            mode: spaghetti_common::ipc_protocol::TapMode::Mirror,
        };
        self.sources.insert(
            id,
            SourceEntry {
                spec,
                tag: None,
                source: Source::Fixture(fixture),
            },
        );
        (id, sinks)
    }

    /// Claude: a ring the source feeds, which the test reads the way a route's
    /// output would, with the timing such an output is built for.
    pub fn attach_fixture_sink(&mut self, source_id: u32) -> (Arc<Ring>, SourceTiming) {
        let source = &mut self.sources.get_mut(&source_id).expect("the source exists").source;
        let ring = Arc::new(Ring::new());
        source.add_sink(Arc::clone(&ring)).expect("a free sink");
        (ring, source.timing().expect("a fixture has timing"))
    }

    pub fn route_ring(&self, route_id: u32) -> Arc<Ring> {
        match &self.routes.get(&route_id).expect("the route exists").path {
            Path::Ring { ring, .. } => Arc::clone(ring),
            Path::Bus { .. } => panic!("route {route_id} is mixed by a bus"),
        }
    }
}
