use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use spaghetti_common::ipc_protocol::{
    AirPlayReceiverInfo, AirPlaySupport, FlowState, FlowStatus, PeerRef,
};

use crate::airplay_sender::AirPlayOutput;
use crate::audio;
use crate::bus::DeviceBus;
use crate::calibration::{
    Calibrating, Calibration, Feed, Job, Leg, Recording, Role, emitter_timing,
};
use crate::device_output;
use crate::flow::{FlowMeter, Link};
use crate::input::InputSource;
use crate::mixer::{Cycles, FADE_LIMIT, IDLE};
use crate::program_io::{InputShared, OutputShared, ProgramInput, ProgramOutput, ProgramSource};
use crate::reservations::{Endpoint, Reservations};
use crate::ring::Ring;
use crate::route_output::{Output, Target};
use crate::sinks::SourceTiming;
use crate::spec::{Destination, Playback, RouteInfo, SourceInfo, SourceSpec};
use crate::srt_receiver::SrtSource;
use crate::tap::TapSource;
use crate::webrtc::PeerLink;
use crate::webrtc_receiver::WebRtcSource;

enum Source {
    Tap(TapSource),
    Srt(SrtSource),
    WebRtc(WebRtcSource),
    Program(ProgramSource),
    Input(InputSource),
}

impl Source {
    fn open(source_id: u32, spec: &SourceSpec, programs: &Programs) -> Result<Self, String> {
        match spec {
            SourceSpec::Tap { pids, mode } => {
                TapSource::create(source_id, pids, *mode).map(Self::Tap)
            }
            SourceSpec::Srt(socket) => SrtSource::open(socket).map(Self::Srt),
            SourceSpec::WebRtc(peer) => WebRtcSource::open(peer).map(Self::WebRtc),
            SourceSpec::Program { input } => programs
                .input(input)
                .map(|shared| Self::Program(ProgramSource::open(shared))),
            SourceSpec::Input { device_uid } => InputSource::create(device_uid).map(Self::Input),
        }
    }

    fn timing(&mut self) -> Result<SourceTiming, String> {
        match self {
            Self::Tap(tap) => tap.timing(),
            Self::Srt(srt) => Ok(srt.timing()),
            Self::WebRtc(webrtc) => Ok(webrtc.timing()),
            Self::Program(program) => Ok(program.timing()),
            Self::Input(input) => input.timing(),
        }
    }

    fn add_sink(&mut self, ring: Arc<Ring>) -> Result<usize, String> {
        match self {
            Self::Tap(tap) => tap.add_sink(ring),
            Self::Srt(srt) => srt.add_sink(ring),
            Self::WebRtc(webrtc) => webrtc.add_sink(ring),
            Self::Program(program) => program.add_sink(ring),
            Self::Input(input) => input.add_sink(ring),
        }
    }

    fn remove_sink(&mut self, slot: usize) {
        match self {
            Self::Tap(tap) => tap.remove_sink(slot),
            Self::Srt(srt) => srt.remove_sink(slot),
            Self::WebRtc(webrtc) => webrtc.remove_sink(slot),
            Self::Program(program) => program.remove_sink(slot),
            Self::Input(input) => input.remove_sink(slot),
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::Tap(tap) => &tap.label,
            Self::Srt(srt) => &srt.label,
            Self::WebRtc(webrtc) => &webrtc.label,
            Self::Program(program) => &program.label,
            Self::Input(input) => &input.label,
        }
    }

    fn status(&self) -> FlowStatus {
        match self {
            Self::Tap(tap) => tap.status(),
            Self::Srt(srt) => srt.status(),
            Self::WebRtc(webrtc) => webrtc.status(),
            Self::Program(program) => program.status(),
            Self::Input(input) => input.status(),
        }
    }

    fn failure(&self) -> Option<String> {
        match self {
            Self::Input(input) => input.failure(),
            Self::Tap(_) | Self::Srt(_) | Self::WebRtc(_) | Self::Program(_) => None,
        }
    }

    fn peer(&self) -> Option<&PeerLink> {
        match self {
            Self::WebRtc(webrtc) => Some(webrtc.peer()),
            _ => None,
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
            Self::Ring { ring, output, .. } => {
                let mut status = output.status();
                if ring.has_ended() {
                    status.state = FlowState::Lost;
                }
                status
            }
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

    fn playing_cycles(&self) -> Option<&Cycles> {
        match self {
            Self::Ring { output, .. } => output.playing_cycles(),
            Self::Bus { cycles, .. } => Some(cycles.as_ref()).filter(|cycles| cycles.running(IDLE)),
        }
    }

    /// Claude: a bus mixes its taps in the cycle they capture, so it plays a
    /// route whenever it runs.
    fn playback(&self) -> Option<Playback> {
        match self {
            Self::Ring { output, .. } => output.playback(),
            Self::Bus { cycles, .. } if cycles.running(IDLE) => Some(Playback::Playing),
            Self::Bus { .. } => Some(Playback::Stopped),
        }
    }
}

/// Claude: the inputs and outputs the embedding program registered, by name.
#[derive(Default)]
struct Programs {
    inputs: HashMap<String, Arc<InputShared>>,
    outputs: HashMap<String, Arc<OutputShared>>,
}

impl Programs {
    fn input(&self, name: &str) -> Result<Arc<InputShared>, String> {
        self.inputs
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no program input '{name}'"))
    }

    fn output(&self, name: &str) -> Result<Arc<OutputShared>, String> {
        self.outputs
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no program output '{name}'"))
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
    finishing: Vec<airplay::Finishing>,
    programs: Programs,
    reservations: Arc<Reservations>,
    /// Claude: cleared when the router drops, which ends every calibration.
    alive: Arc<AtomicBool>,
    next_source_id: u32,
    next_route_id: u32,
}

impl Router {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
            sources: HashMap::new(),
            buses: HashMap::new(),
            finishing: Vec::new(),
            programs: Programs::default(),
            reservations: Arc::default(),
            alive: Arc::new(AtomicBool::new(true)),
            next_source_id: 1,
            next_route_id: 1,
        }
    }

    pub fn create_source(&mut self, spec: SourceSpec, tag: Option<String>) -> Result<u32, String> {
        let id = self.next_source_id;
        let source = Source::open(id, &spec, &self.programs)?;
        self.next_source_id += 1;
        tracing::info!(
            source_id = id,
            label = source.label(),
            ?tag,
            "created source"
        );
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
        for route in self
            .routes
            .values()
            .filter(|route| route.source_id == source_id)
        {
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
                failure: entry.source.failure(),
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
        mut destination: Destination,
        initial_volume: f32,
    ) -> Result<u32, String> {
        let entry = self
            .sources
            .get_mut(&source_id)
            .ok_or_else(|| format!("source {source_id} not found"))?;

        validate_schedule_source(&entry.spec, &destination)?;
        let volume = Arc::new(AtomicU32::new(initial_volume.clamp(0.0, 2.0).to_bits()));
        let path = match (&entry.spec, &destination) {
            (
                SourceSpec::Tap { pids, mode },
                Destination::Device {
                    uid,
                    delay_ms: None,
                    latency_offset_ms: None,
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
                let timing = source.timing()?;
                let target = resolve(
                    &mut destination,
                    &timing,
                    &self.programs,
                    &self.reservations,
                    self.routes
                        .iter()
                        .map(|(&id, route)| (id, &route.destination)),
                )?;
                let (output, ring) = Output::open(target, timing, Arc::clone(&volume))?;
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
        if let Some(cycles) = path.playing_cycles() {
            volume.store(0f32.to_bits(), Ordering::Release);
            cycles.wait_fade(3, FADE_LIMIT);
        }

        match (path, destination) {
            (
                Path::Ring {
                    sink_slot, output, ..
                },
                destination,
            ) => {
                match (output, destination) {
                    (Output::AirPlay(output), Destination::AirPlay { .. }) => {
                        self.finishing.push(output.finish());
                    }
                    (output, _) => drop(output),
                }
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

    pub fn fade_volume(&mut self, route_id: u32, volume: f32) -> Result<(), String> {
        self.set_volume(route_id, volume)?;
        let cycles = self
            .routes
            .get(&route_id)
            .and_then(|route| route.path.playing_cycles());
        if let Some(cycles) = cycles {
            cycles.wait_fade(2, FADE_LIMIT);
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
                playback: r.path.playback(),
                local_description: r.path.peer().and_then(PeerLink::local_description),
            })
            .collect()
    }

    pub fn route_count(&self) -> u32 {
        self.routes.len() as u32
    }

    pub fn log_health(&mut self) {
        self.finishing.retain(|finishing| !finishing.is_finished());
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

    /// Claude: starts measuring how much later than `calibration.reference`
    /// its output sounds where the recording is made, and returns at once.
    /// It checks the destinations as `create_route` would for a source of
    /// the calibration's own, and holds an AirPlay receiver or program output
    /// it plays to against other routes until it ends; everything that can
    /// block opens on its own thread.
    pub fn calibrate(&mut self, calibration: Calibration) -> Result<Calibrating, String> {
        Calibrating::start(self.calibration_job(calibration)?)
    }

    pub(crate) fn calibration_job(&mut self, calibration: Calibration) -> Result<Job, String> {
        let Calibration {
            output,
            reference,
            recording,
        } = calibration;
        let (reference, reference_endpoint) = self.calibration_leg(Role::Reference, reference)?;
        let (output, output_endpoint) = self.calibration_leg(Role::Output, output)?;
        let endpoints: Vec<Endpoint> = [reference_endpoint, output_endpoint]
            .into_iter()
            .flatten()
            .collect();
        if let [a, b] = &endpoints[..]
            && a == b
        {
            return Err(format!("the output and the reference are both {a}"));
        }
        let feed = match recording {
            Recording::Input { device_uid } => Feed::Input(InputSource::create(&device_uid)?),
            Recording::Program { input } => Feed::Program(self.programs.input(&input)?),
        };
        let reservation = self.reservations.reserve(endpoints)?;
        Ok(Job::new(
            reference,
            output,
            feed,
            reservation,
            Arc::clone(&self.alive),
        ))
    }

    /// Claude: a calibration's leg to `destination`, which must play on a
    /// delay, and the endpoint the calibration holds for it, if any.
    fn calibration_leg(
        &self,
        role: Role,
        mut destination: Destination,
    ) -> Result<(Leg, Option<Endpoint>), String> {
        let (delay_ms, latency_offset_ms) = match &destination {
            Destination::Device {
                delay_ms: Some(delay_ms),
                latency_offset_ms,
                ..
            }
            | Destination::Program {
                delay_ms: Some(delay_ms),
                latency_offset_ms,
                ..
            }
            | Destination::AirPlay {
                delay_ms,
                latency_offset_ms,
                ..
            } => (*delay_ms, *latency_offset_ms),
            _ => {
                return Err(format!(
                    "the {role} must play on a delay: a device or program output with one, or an \
                     AirPlay receiver"
                ));
            }
        };
        if let Destination::Program { output, .. } = &destination
            && let Some((route_id, _)) = self.routes.iter().find(|(_, route)| {
                matches!(&route.destination, Destination::Program { output: used, .. } if used == output)
            })
        {
            return Err(format!("program output '{output}' already has route {route_id}"));
        }
        let timing = emitter_timing();
        let target = resolve(
            &mut destination,
            &timing,
            &self.programs,
            &self.reservations,
            self.routes
                .iter()
                .map(|(&id, route)| (id, &route.destination)),
        )?;
        let delay = f64::from(delay_ms) / 1000.0;
        match &target {
            Target::Device { device, .. } => {
                device_output::check_schedule(*device, &timing, delay, latency_offset_ms)?
            }
            Target::Program { output, .. } => {
                output.check_schedule(&timing, delay, latency_offset_ms)?
            }
            Target::AirPlay { .. } | Target::Srt(_) | Target::WebRtc(_) => {}
        }
        let endpoint = match destination {
            Destination::AirPlay { receiver, .. } => Some(Endpoint::AirPlay(receiver)),
            Destination::Program { output, .. } => Some(Endpoint::ProgramOutput(output)),
            _ => None,
        };
        Ok((Leg::new(role, target, delay, latency_offset_ms), endpoint))
    }

    /// Return every route ID currently targeting the given device UID.
    pub fn routes_targeting(&self, device_uid: &str) -> Vec<u32> {
        self.routes
            .iter()
            .filter(|(_, r)| matches!(&r.destination, Destination::Device { uid, .. } if uid == device_uid))
            .map(|(&id, _)| id)
            .collect()
    }

    /// Claude: registers audio the program captures itself at `sample_rate`,
    /// which `SourceSpec::Program` names from now on. The program pushes it
    /// through the returned input, from its capture callback.
    pub fn add_program_input(
        &mut self,
        name: &str,
        sample_rate: f64,
    ) -> Result<ProgramInput, String> {
        let Entry::Vacant(vacant) = self.programs.inputs.entry(name.to_string()) else {
            return Err(format!("program input '{name}' already exists"));
        };
        let shared = InputShared::new(name, sample_rate)?;
        vacant.insert(Arc::clone(&shared));
        tracing::info!(name, sample_rate, "added program input");
        Ok(ProgramInput::new(shared))
    }

    /// Claude: registers an output the program plays itself at `sample_rate`,
    /// which `Destination::Program` names from now on. The program renders it
    /// through the returned output, from its playback callback. `latency` is
    /// the least time from a render to its first frame being heard, which
    /// bounds the delays its routes can hold.
    pub fn add_program_output(
        &mut self,
        name: &str,
        sample_rate: f64,
        latency: Duration,
    ) -> Result<ProgramOutput, String> {
        let Entry::Vacant(vacant) = self.programs.outputs.entry(name.to_string()) else {
            return Err(format!("program output '{name}' already exists"));
        };
        let shared = OutputShared::new(sample_rate, latency)?;
        vacant.insert(Arc::clone(&shared));
        tracing::info!(name, sample_rate, ?latency, "added program output");
        Ok(ProgramOutput::new(shared))
    }

    /// Claude: destroys the sources made from the input, then forgets it. Its
    /// real-time end keeps working and feeds nothing.
    pub fn remove_program_input(&mut self, name: &str) -> Result<(), String> {
        if !self.programs.inputs.contains_key(name) {
            return Err(format!("no program input '{name}'"));
        }
        let sources: Vec<u32> = self
            .sources
            .iter()
            .filter(
                |(_, entry)| matches!(&entry.spec, SourceSpec::Program { input } if input == name),
            )
            .map(|(&id, _)| id)
            .collect();
        for source_id in sources {
            self.destroy_source(source_id)?;
        }
        if let Some(input) = self.programs.inputs.remove(name) {
            input.end();
        }
        Ok(())
    }

    /// Claude: destroys the routes to the output, fading out those it plays,
    /// then forgets it. Its real-time end keeps rendering silence.
    pub fn remove_program_output(&mut self, name: &str) -> Result<(), String> {
        if !self.programs.outputs.contains_key(name) {
            return Err(format!("no program output '{name}'"));
        }
        let routes: Vec<u32> = self
            .routes
            .iter()
            .filter(|(_, route)| matches!(&route.destination, Destination::Program { output, .. } if output == name))
            .map(|(&id, _)| id)
            .collect();
        for route_id in routes {
            self.destroy_route(route_id)?;
        }
        if let Some(output) = self.programs.outputs.remove(name) {
            output.end();
        }
        Ok(())
    }
}

pub fn list_airplay_receivers() -> Result<Vec<AirPlayReceiverInfo>, String> {
    let mut receivers: Vec<_> = airplay_browser()
        .receivers()?
        .into_iter()
        .map(|receiver| {
            let support = match receiver.support() {
                Ok(()) => AirPlaySupport::Supported,
                Err(reason) => AirPlaySupport::Unsupported { reason },
            };
            AirPlayReceiverInfo {
                id: receiver.id.to_string(),
                name: receiver.name,
                model: receiver.model,
                support,
            }
        })
        .collect();
    receivers.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    Ok(receivers)
}

/// Claude: where `destination` plays for a source of `timing`, beside the
/// routes the router has. An AirPlay receiver's ID is written back the way
/// the route lists it.
fn resolve<'a>(
    destination: &mut Destination,
    timing: &SourceTiming,
    programs: &Programs,
    reservations: &Reservations,
    routes: impl Iterator<Item = (u32, &'a Destination)>,
) -> Result<Target, String> {
    let delay = |ms: &Option<u32>| ms.map(|ms| f64::from(ms) / 1000.0);
    Ok(match destination {
        Destination::Device {
            uid,
            delay_ms,
            latency_offset_ms,
        } => Target::Device {
            device: audio::find_device_by_uid(uid)
                .ok_or_else(|| format!("device '{uid}' not found"))?,
            delay: delay(delay_ms),
            latency_offset_ms: *latency_offset_ms,
        },
        Destination::AirPlay {
            receiver,
            delay_ms,
            latency_offset_ms,
        } => {
            let playout = AirPlayOutput::playout(timing, *delay_ms, *latency_offset_ms)?;
            let id: airplay::ReceiverId = receiver
                .parse()
                .map_err(|reason| format!("invalid AirPlay receiver id '{receiver}': {reason}"))?;
            validate_receiver_available(id.as_str(), routes)?;
            reservations.check(&Endpoint::AirPlay(id.to_string()))?;
            *receiver = id.to_string();
            Target::AirPlay {
                receiver: id,
                playout: Box::new(playout),
            }
        }
        Destination::Program {
            output,
            delay_ms,
            latency_offset_ms,
        } => {
            reservations.check(&Endpoint::ProgramOutput(output.clone()))?;
            Target::Program {
                output: programs.output(output)?,
                delay: delay(delay_ms),
                latency_offset_ms: *latency_offset_ms,
            }
        }
        Destination::Srt(socket) => Target::Srt(socket.clone()),
        Destination::WebRtc(peer) => Target::WebRtc(peer.clone()),
    })
}

pub(crate) fn airplay_browser() -> &'static airplay::Browser {
    static BROWSER: OnceLock<airplay::Browser> = OnceLock::new();
    BROWSER.get_or_init(airplay::Browser::start)
}

fn validate_schedule_source(source: &SourceSpec, destination: &Destination) -> Result<(), String> {
    let scheduled = match destination {
        Destination::Device {
            delay_ms,
            latency_offset_ms,
            ..
        }
        | Destination::Program {
            delay_ms,
            latency_offset_ms,
            ..
        } => {
            if delay_ms.is_none() && latency_offset_ms.is_some() {
                return Err("a latency offset requires a delay".into());
            }
            delay_ms.is_some()
        }
        Destination::AirPlay { .. } => true,
        Destination::Srt(_) | Destination::WebRtc(_) => false,
    };
    let dated = match source {
        SourceSpec::Tap { .. }
        | SourceSpec::WebRtc(_)
        | SourceSpec::Program { .. }
        | SourceSpec::Input { .. } => true,
        SourceSpec::Srt(_) => false,
    };
    if scheduled && !dated {
        return Err(
            "an SRT source does not date its frames, so it cannot play on a schedule".into(),
        );
    }
    Ok(())
}

fn validate_receiver_available<'a>(
    receiver: &str,
    destinations: impl Iterator<Item = (u32, &'a Destination)>,
) -> Result<(), String> {
    for (route_id, destination) in destinations {
        if let Destination::AirPlay { receiver: used, .. } = destination
            && receiver == used
        {
            return Err(format!(
                "AirPlay receiver '{receiver}' already has route {route_id}"
            ));
        }
    }
    Ok(())
}

impl Drop for Router {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        let routes: Vec<_> = self.routes.keys().copied().collect();
        for route in routes {
            let _ = self.destroy_route(route);
        }
        for output in self.programs.outputs.values() {
            output.end();
        }
        for input in self.programs.inputs.values() {
            input.end();
        }
        for finishing in self.finishing.drain(..) {
            finishing.wait();
        }
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl Router {
    /// Claude: a ring the source feeds, which the test reads the way a route's
    /// output would, with the timing such an output is built for.
    pub fn attach_raw_sink(&mut self, source_id: u32) -> (Arc<Ring>, SourceTiming) {
        let source = &mut self
            .sources
            .get_mut(&source_id)
            .expect("the source exists")
            .source;
        let timing = source.timing().expect("the source has timing");
        let ring = Arc::new(Ring::new(timing.sample_rate));
        source.add_sink(Arc::clone(&ring)).expect("a free sink");
        (ring, timing)
    }

    /// Claude: whether nothing, route or calibration, plays to the output.
    pub fn program_output_idle(&self, name: &str) -> bool {
        self.programs.outputs[name].is_idle()
    }

    /// Claude: whether nothing, source or calibration, takes the input.
    pub fn program_input_idle(&self, name: &str) -> bool {
        self.programs.inputs[name].is_idle()
    }

    pub fn route_ring(&self, route_id: u32) -> Arc<Ring> {
        match &self.routes.get(&route_id).expect("the route exists").path {
            Path::Ring { ring, .. } => Arc::clone(ring),
            Path::Bus { .. } => panic!("route {route_id} is mixed by a bus"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spaghetti_common::ipc_protocol::{SrtMode, SrtSocket, TapMode};

    fn airplay(receiver: &str, delay_ms: u32) -> Destination {
        Destination::AirPlay {
            receiver: receiver.into(),
            delay_ms,
            latency_offset_ms: None,
        }
    }

    #[test]
    fn undated_sources_are_rejected_for_every_scheduled_destination() {
        let source = SourceSpec::Srt(SrtSocket {
            mode: SrtMode::Listen { port: 9000 },
            latency_ms: 200,
            passphrase: None,
            key_size: None,
        });
        for destination in [
            airplay("C4:38:75:04:5E:80", 2500),
            Destination::Device {
                uid: "Speakers".into(),
                delay_ms: Some(2500),
                latency_offset_ms: None,
            },
            Destination::Program {
                output: "playback".into(),
                delay_ms: Some(2500),
                latency_offset_ms: None,
            },
        ] {
            assert!(
                validate_schedule_source(&source, &destination)
                    .unwrap_err()
                    .contains("does not date")
            );
            let tap = SourceSpec::Tap {
                pids: vec![],
                mode: TapMode::Mirror,
            };
            assert!(validate_schedule_source(&tap, &destination).is_ok());
            let program = SourceSpec::Program {
                input: "capture".into(),
            };
            assert!(validate_schedule_source(&program, &destination).is_ok());
        }
        assert!(
            validate_schedule_source(
                &source,
                &Destination::Device {
                    uid: "Speakers".into(),
                    delay_ms: None,
                    latency_offset_ms: None
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn latency_offsets_require_a_delay_before_opening_the_output() {
        let mut router = Router::new();
        let _input = router.add_program_input("capture", 48000.0).unwrap();
        let source = router
            .create_source(
                SourceSpec::Program {
                    input: "capture".into(),
                },
                None,
            )
            .unwrap();
        for offset in [-47, 0, 83] {
            for destination in [
                Destination::Device {
                    uid: "absent".into(),
                    delay_ms: None,
                    latency_offset_ms: Some(offset),
                },
                Destination::Program {
                    output: "absent".into(),
                    delay_ms: None,
                    latency_offset_ms: Some(offset),
                },
            ] {
                let error = router.create_route(source, destination, 1.0).unwrap_err();
                assert!(error.contains("latency offset requires a delay"), "{error}");
            }
        }
        assert_eq!(router.route_count(), 0);
    }

    #[test]
    fn latency_offsets_shift_airplay_delay_bounds_before_discovery() {
        let mut router = Router::new();
        let _input = router.add_program_input("capture", 48000.0).unwrap();
        let source = router
            .create_source(
                SourceSpec::Program {
                    input: "capture".into(),
                },
                None,
            )
            .unwrap();
        let timing = router
            .sources
            .get_mut(&source)
            .unwrap()
            .source
            .timing()
            .unwrap();
        let plain = AirPlayOutput::playout(&timing, 2500, None).unwrap();
        let minimum_ms = plain.minimum_delay(airplay::LATENCY.as_secs_f64()) * 1000.0;
        let maximum_ms = plain.max_held_seconds() * 1000.0;
        for offset in [-47, 0, 83] {
            let minimum = (minimum_ms + f64::from(offset)).ceil() as u32;
            let maximum = (maximum_ms + f64::from(offset)).floor() as u32;
            assert!(AirPlayOutput::playout(&timing, minimum, Some(offset)).is_ok());
            assert!(AirPlayOutput::playout(&timing, maximum, Some(offset)).is_ok());
            for (delay_ms, message) in
                [(minimum - 1, "at least"), (maximum + 1, "held-audio limit")]
            {
                let destination = Destination::AirPlay {
                    receiver: "invalid-receiver".into(),
                    delay_ms,
                    latency_offset_ms: Some(offset),
                };
                let error = router.create_route(source, destination, 1.0).unwrap_err();
                assert!(
                    error.contains(message),
                    "{offset} ms offset, {delay_ms} ms delay: {error}"
                );
            }
        }
        assert_eq!(router.route_count(), 0);
    }

    #[test]
    fn a_route_whose_source_ended_is_lost() {
        let mut router = Router::new();
        let _input = router.add_program_input("capture", 48000.0).unwrap();
        let _output = router
            .add_program_output("playback", 48000.0, Duration::ZERO)
            .unwrap();
        let source = router
            .create_source(
                SourceSpec::Program {
                    input: "capture".into(),
                },
                None,
            )
            .unwrap();
        let route = router
            .create_route(
                source,
                Destination::Program {
                    output: "playback".into(),
                    delay_ms: None,
                    latency_offset_ms: None,
                },
                1.0,
            )
            .unwrap();
        let state = |router: &Router| router.list_routes()[0].status.state;
        assert_eq!(state(&router), FlowState::Connected);
        router.route_ring(route).end();
        assert_eq!(state(&router), FlowState::Lost);
    }

    #[test]
    fn a_receiver_accepts_only_one_route() {
        let destinations = [airplay("C4:38:75:04:5E:80", 2500)];
        let existing = || destinations.iter().map(|d| (9, d));
        assert!(
            validate_receiver_available("C4:38:75:04:5E:80", existing())
                .unwrap_err()
                .contains("already has route 9")
        );
        assert!(validate_receiver_available("C4:38:75:04:5E:81", existing()).is_ok());
        assert!(validate_receiver_available("C4:38:75:04:5E:80", std::iter::empty()).is_ok());
    }

    #[test]
    fn airplay_delay_rejections_happen_before_any_discovery_or_session() {
        for rate in [48000.0, 96000.0] {
            let mut router = Router::new();
            let _input = router.add_program_input("capture", rate).unwrap();
            let source = router
                .create_source(
                    SourceSpec::Program {
                        input: "capture".into(),
                    },
                    None,
                )
                .unwrap();
            for delay in [0, 1999, 2000, 2050] {
                let error = router
                    .create_route(source, airplay("C4:38:75:04:5E:80", delay), 1.0)
                    .unwrap_err();
                assert!(error.contains("at least"), "{rate} Hz, {delay} ms: {error}");
            }
            let error = router
                .create_route(source, airplay("C4:38:75:04:5E:80", 10000), 1.0)
                .unwrap_err();
            assert!(error.contains("held-audio limit"), "{error}");
            let timing = router
                .sources
                .get_mut(&source)
                .unwrap()
                .source
                .timing()
                .unwrap();
            assert!(AirPlayOutput::playout(&timing, 2200, None).is_ok());
            assert!(AirPlayOutput::playout(&timing, 2500, None).is_ok());
            assert_eq!(router.route_count(), 0);
        }
    }
}
