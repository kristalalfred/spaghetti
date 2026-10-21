use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use coreaudio_sys::*;
use spaghetti_common::ipc_protocol::RouteInfo;

use crate::mixer;
use crate::playout::{Playout, PlayoutHealth};
use crate::ring::{CHANNELS, Ring};
use crate::rt_slots::RtSlots;

/// A live audio route: reads from a tap's aggregate device, writes to a virtual device.
struct Route {
    route_id: u32,
    tap_id: u32,
    device_uid: String,
    volume: Arc<AtomicU32>,
    playback: Playback,
}

/// Claude: what a route owns once it is attached to its tap's capture.
struct Playback {
    sink_slot: usize,
    output: OutputUnit,
    ring: Arc<Ring>,
    health: Arc<PlayoutHealth>,
    reported_glitches: [u32; 3],
}

/// Manages audio routes between taps and virtual devices.
pub struct Router {
    routes: HashMap<u32, Route>,
    captures: HashMap<u32, TapCapture>,
    next_id: u32,
}

impl Router {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
            captures: HashMap::new(),
            next_id: 1,
        }
    }

    pub fn create_route(
        &mut self,
        tap_id: u32,
        source_aggregate_device: AudioObjectID,
        dest_device_uid: &str,
        dest_device_id: AudioObjectID,
        initial_volume: f32,
    ) -> Result<u32, String> {
        let capture = match self.captures.entry(tap_id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(unsafe { TapCapture::open(source_aggregate_device) }?),
        };

        let volume = Arc::new(AtomicU32::new(initial_volume.to_bits()));
        let attached = unsafe { attach_route(capture, dest_device_id, Arc::clone(&volume)) };
        if capture.is_idle() {
            self.captures.remove(&tap_id);
        }
        let playback = attached?;

        let id = self.next_id;
        self.next_id += 1;
        self.routes.insert(
            id,
            Route {
                route_id: id,
                tap_id,
                device_uid: dest_device_uid.to_string(),
                volume,
                playback,
            },
        );

        tracing::info!(route_id = id, tap_id, dest = dest_device_uid, "created route");
        Ok(id)
    }

    pub fn destroy_route(&mut self, route_id: u32) -> Result<(), String> {
        let Route {
            tap_id, playback, ..
        } = self
            .routes
            .remove(&route_id)
            .ok_or_else(|| format!("route {route_id} not found"))?;

        drop(playback.output);
        if let Some(capture) = self.captures.get_mut(&tap_id) {
            capture.remove_sink(playback.sink_slot);
            if capture.is_idle() {
                self.captures.remove(&tap_id);
                tracing::info!(tap_id, "stopped tap capture");
            }
        }

        tracing::info!(route_id, "destroyed route");
        Ok(())
    }

    pub fn destroy_routes_from_tap(&mut self, tap_id: u32) {
        let route_ids: Vec<u32> = self
            .routes
            .values()
            .filter(|r| r.tap_id == tap_id)
            .map(|r| r.route_id)
            .collect();
        for route_id in route_ids {
            let _ = self.destroy_route(route_id);
        }
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
            .values()
            .map(|r| RouteInfo {
                route_id: r.route_id,
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
        for route in self.routes.values_mut() {
            let playback = &mut route.playback;
            let report = playback.health.report();
            let glitches = [report.underruns, report.resyncs, playback.ring.overflows()];
            let to_ms = |frames: f64| frames * 1000.0 / playback.health.source_rate;
            tracing::debug!(
                route_id = route.route_id,
                fill_frames = report.fill_avg_frames,
                fill_ms = to_ms(report.fill_avg_frames as f64),
                target_ms = to_ms(report.target_frames as f64),
                underruns = glitches[0],
                resyncs = glitches[1],
                overflows = glitches[2],
                "route playout health"
            );
            if glitches != playback.reported_glitches {
                tracing::info!(
                    route_id = route.route_id,
                    underruns = glitches[0] - playback.reported_glitches[0],
                    resyncs = glitches[1] - playback.reported_glitches[1],
                    overflows = glitches[2] - playback.reported_glitches[2],
                    "route playout glitched since the last report"
                );
                playback.reported_glitches = glitches;
            }
        }
    }

    /// Return every route ID currently targeting the given device UID.
    pub fn routes_targeting(&self, device_uid: &str) -> Vec<u32> {
        self.routes
            .values()
            .filter(|r| r.device_uid == device_uid)
            .map(|r| r.route_id)
            .collect()
    }
}

// ── AudioBufferList access ────────────────────────────────────────────────────

/// Claude: `mBuffers` is declared `[AudioBuffer; 1]`, so indexing past 0 through
/// it is out of provenance; derive the buffer pointer from the list's base instead.
unsafe fn abl_buffers<'a>(abl: *const AudioBufferList) -> &'a [AudioBuffer] {
    unsafe {
        let first = abl
            .cast::<u8>()
            .add(std::mem::offset_of!(AudioBufferList, mBuffers))
            .cast::<AudioBuffer>();
        std::slice::from_raw_parts(first, (*abl).mNumberBuffers as usize)
    }
}

unsafe fn abl_buffers_mut<'a>(abl: *mut AudioBufferList) -> &'a mut [AudioBuffer] {
    unsafe {
        let first = abl
            .cast::<u8>()
            .add(std::mem::offset_of!(AudioBufferList, mBuffers))
            .cast::<AudioBuffer>();
        std::slice::from_raw_parts_mut(first, (*abl).mNumberBuffers as usize)
    }
}

unsafe fn buffer_samples<'a>(buffer: &AudioBuffer) -> Option<&'a [f32]> {
    (!buffer.mData.is_null()).then(|| unsafe {
        std::slice::from_raw_parts(
            buffer.mData as *const f32,
            buffer.mDataByteSize as usize / std::mem::size_of::<f32>(),
        )
    })
}

// ── Capture side: one IOProc per tap ──────────────────────────────────────────

unsafe fn query_device_stream_format(
    device: AudioObjectID,
) -> Result<AudioStreamBasicDescription, String> {
    let mut format: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreamFormat,
        mScope: kAudioDevicePropertyScopeInput,
        mElement: kAudioObjectPropertyElementMain,
    };
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            &address,
            0,
            std::ptr::null(),
            &mut size,
            &mut format as *mut _ as *mut _,
        )
    };
    if status != 0 {
        return Err(format!("get device stream format failed: {status:#010x}"));
    }
    Ok(format)
}

const MAX_ROUTES_PER_TAP: usize = 8;

type SinkSlots = RtSlots<Sink, MAX_ROUTES_PER_TAP>;

/// Capture uses `AudioDeviceCreateIOProcID` + `AudioDeviceStart` directly on the
/// tap aggregate device. This is the pattern Apple's `AudioCap` sample uses for
/// process taps and avoids `-10863 kAudioUnitErr_CannotDoInCurrentContext` from
/// pulling tap frames through an AUHAL input unit.
///
/// Claude: a tap aggregate runs only one IOProc. Starting a second one halts IO
/// for every IOProc on the device, and removing it does not resume IO, so all
/// routes on a tap share this IOProc and get their frames through `sinks`.
struct TapCapture {
    device: AudioObjectID,
    format: AudioStreamBasicDescription,
    ioproc_id: AudioDeviceIOProcID,
    sinks: Arc<SinkSlots>,
    running: bool,
}

impl TapCapture {
    unsafe fn open(device: AudioObjectID) -> Result<Self, String> {
        let format = unsafe { query_device_stream_format(device) }?;
        tracing::info!(
            sample_rate = format.mSampleRate,
            channels_per_frame = format.mChannelsPerFrame,
            bytes_per_frame = format.mBytesPerFrame,
            frames_per_packet = format.mFramesPerPacket,
            bits_per_channel = format.mBitsPerChannel,
            format_flags = format.mFormatFlags,
            is_float = (format.mFormatFlags & kAudioFormatFlagIsFloat) != 0,
            is_non_interleaved = (format.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0,
            "tap ASBD (source aggregate device input scope)"
        );
        if format.mSampleRate <= 0.0
            || format.mFormatID != kAudioFormatLinearPCM
            || format.mFormatFlags & kAudioFormatFlagIsFloat == 0
            || format.mBitsPerChannel != 32
        {
            return Err(format!(
                "unsupported tap stream format (rate={}, format={:#x}, flags={:#x}, bits={})",
                format.mSampleRate, format.mFormatID, format.mFormatFlags, format.mBitsPerChannel
            ));
        }

        let sinks = Arc::new(SinkSlots::new());
        let mut ioproc_id: AudioDeviceIOProcID = None;
        let status = unsafe {
            AudioDeviceCreateIOProcID(
                device,
                Some(capture_ioproc),
                Arc::as_ptr(&sinks).cast_mut().cast(),
                &mut ioproc_id,
            )
        };
        if status != 0 || ioproc_id.is_none() {
            return Err(format!("AudioDeviceCreateIOProcID failed: {status:#010x}"));
        }

        Ok(Self {
            device,
            format,
            ioproc_id,
            sinks,
            running: false,
        })
    }

    /// Claude: starts the IOProc when the first sink arrives.
    unsafe fn add_sink(&mut self, sink: Sink) -> Result<usize, String> {
        let slot = self
            .sinks
            .insert(Box::new(sink))
            .map_err(|_| format!("a tap can feed at most {MAX_ROUTES_PER_TAP} routes"))?;

        if !self.running {
            let status = unsafe { AudioDeviceStart(self.device, self.ioproc_id) };
            if status != 0 {
                self.sinks.remove(slot);
                return Err(format!("AudioDeviceStart (IOProc) failed: {status:#010x}"));
            }
            self.running = true;
        }
        Ok(slot)
    }

    fn remove_sink(&mut self, slot: usize) {
        drop(self.sinks.remove(slot));
    }

    fn is_idle(&self) -> bool {
        self.sinks.is_empty()
    }
}

impl Drop for TapCapture {
    fn drop(&mut self) {
        unsafe {
            if self.running {
                AudioDeviceStop(self.device, self.ioproc_id);
            }
            AudioDeviceDestroyIOProcID(self.device, self.ioproc_id);
        }
    }
}

/// Claude: one route's share of a tap capture, feeding that route's ring at the tap's rate.
struct Sink {
    ring: Arc<Ring>,
    logged_once: bool,
}

impl Sink {
    /// Claude: `input` must be the ABL the HAL handed to the IOProc. Taps deliver
    /// interleaved stereo in practice whatever their ASBD says, so both layouts are accepted.
    unsafe fn push(&mut self, input: *const AudioBufferList, now: u64) {
        let buffers = unsafe { abl_buffers(input) };
        let Some(first) = buffers.first() else {
            return;
        };

        let frames = if let [left, right, ..] = buffers {
            let frames = left.mDataByteSize.min(right.mDataByteSize) as usize
                / std::mem::size_of::<f32>();
            let planes = unsafe { [buffer_samples(left), buffer_samples(right)] };
            self.ring
                .push_with(frames, now, |f, ch| planes[ch].map_or(0.0, |s| s[f]));
            frames
        } else {
            let channels = first.mNumberChannels.max(1) as usize;
            let frames = first.mDataByteSize as usize / (channels * std::mem::size_of::<f32>());
            let samples = unsafe { buffer_samples(first) };
            self.ring.push_with(frames, now, |f, ch| {
                samples.map_or(0.0, |s| s[f * channels + ch.min(channels - 1)])
            });
            frames
        };

        if !self.logged_once {
            self.logged_once = true;
            tracing::info!(
                in_n_buffers = buffers.len(),
                in_buf0_channels = first.mNumberChannels,
                in_buf0_bytes = first.mDataByteSize,
                in_buf0_null = first.mData.is_null(),
                frames,
                "IOProc first call"
            );
        }
    }
}

unsafe extern "C" fn capture_ioproc(
    _in_device: AudioObjectID,
    _in_now: *const AudioTimeStamp,
    in_input_data: *const AudioBufferList,
    _in_input_time: *const AudioTimeStamp,
    _out_output_data: *mut AudioBufferList,
    _in_output_time: *const AudioTimeStamp,
    in_client_data: *mut std::ffi::c_void,
) -> OSStatus {
    if in_input_data.is_null() || in_client_data.is_null() {
        return 0;
    }

    let now = unsafe { AudioGetCurrentHostTime() };
    let sinks = unsafe { &*(in_client_data as *const SinkSlots) };
    unsafe { sinks.for_each(|sink| sink.push(in_input_data, now)) };
    0
}

// ── Output side: one AUHAL per route ──────────────────────────────────────────

struct OutputCallbackState {
    ring: Arc<Ring>,
    playout: Playout,
    volume: Arc<AtomicU32>,
    logged_once: bool,
}

struct OutputUnit {
    unit: AudioUnit,
    /// Claude: leaked Box handed to the render callback; freed after the unit is disposed.
    render_state: *mut OutputCallbackState,
}

// Claude: the unit is driven from one control thread at a time, and the render
// state is only touched by the render callback while the unit exists.
unsafe impl Send for OutputUnit {}

impl OutputUnit {
    unsafe fn new(device_id: AudioObjectID) -> Result<Self, String> {
        let desc = AudioComponentDescription {
            componentType: kAudioUnitType_Output,
            componentSubType: kAudioUnitSubType_HALOutput,
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0,
        };

        let comp = unsafe { AudioComponentFindNext(std::ptr::null_mut(), &desc) };
        if comp.is_null() {
            return Err("no HALOutput component found".into());
        }

        let mut unit: AudioUnit = std::ptr::null_mut();
        let status = unsafe { AudioComponentInstanceNew(comp, &mut unit) };
        if status != 0 {
            return Err(format!("AudioComponentInstanceNew failed: {status}"));
        }
        let output = Self {
            unit,
            render_state: std::ptr::null_mut(),
        };

        let status = unsafe {
            AudioUnitSetProperty(
                unit,
                kAudioOutputUnitProperty_CurrentDevice,
                kAudioUnitScope_Global,
                0,
                &device_id as *const AudioObjectID as *const _,
                std::mem::size_of::<AudioObjectID>() as u32,
            )
        };
        if status != 0 {
            return Err(format!("set output device failed: {status}"));
        }

        Ok(output)
    }

    /// Claude: the output scope is the device side of the unit. The input scope
    /// is the client side and defaults to 44.1 kHz whatever the device runs at.
    unsafe fn device_format(&self) -> Result<AudioStreamBasicDescription, String> {
        let mut format: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
        let status = unsafe {
            AudioUnitGetProperty(
                self.unit,
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Output,
                0,
                &mut format as *mut _ as *mut _,
                &mut size,
            )
        };
        if status != 0 {
            return Err(format!("get output device format failed: {status}"));
        }
        if format.mSampleRate <= 0.0 {
            return Err(format!("output device reports sample rate {}", format.mSampleRate));
        }
        Ok(format)
    }

    unsafe fn set_client_format(&self, format: &AudioStreamBasicDescription) -> Result<(), String> {
        let status = unsafe {
            AudioUnitSetProperty(
                self.unit,
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Input,
                0,
                format as *const AudioStreamBasicDescription as *const _,
                std::mem::size_of::<AudioStreamBasicDescription>() as u32,
            )
        };
        if status != 0 {
            return Err(format!("set output unit client format failed: {status}"));
        }
        Ok(())
    }

    unsafe fn connect(
        &mut self,
        ring: Arc<Ring>,
        playout: Playout,
        volume: Arc<AtomicU32>,
    ) -> Result<(), String> {
        self.render_state = Box::into_raw(Box::new(OutputCallbackState {
            ring,
            playout,
            volume,
            logged_once: false,
        }));

        let render_cb = AURenderCallbackStruct {
            inputProc: Some(route_render_callback),
            inputProcRefCon: self.render_state.cast(),
        };
        let status = unsafe {
            AudioUnitSetProperty(
                self.unit,
                kAudioUnitProperty_SetRenderCallback,
                kAudioUnitScope_Input,
                0,
                &render_cb as *const _ as *const _,
                std::mem::size_of::<AURenderCallbackStruct>() as u32,
            )
        };
        if status != 0 {
            return Err(format!("set render callback failed: {status}"));
        }

        let status = unsafe { AudioUnitInitialize(self.unit) };
        if status != 0 {
            return Err(format!("initialize output unit failed: {status}"));
        }
        Ok(())
    }

    unsafe fn start(&self) -> Result<(), String> {
        let status = unsafe { AudioOutputUnitStart(self.unit) };
        if status != 0 {
            return Err(format!("start output unit failed: {status}"));
        }
        Ok(())
    }
}

impl Drop for OutputUnit {
    fn drop(&mut self) {
        unsafe {
            AudioOutputUnitStop(self.unit);
            AudioUnitUninitialize(self.unit);
            AudioComponentInstanceDispose(self.unit);
            if !self.render_state.is_null() {
                drop(Box::from_raw(self.render_state));
            }
        }
    }
}

fn output_planes(buffers: &mut [AudioBuffer], frames: usize) -> Option<[&mut [f32]; CHANNELS]> {
    let [left, right] = buffers else {
        return None;
    };
    let plane = |buffer: &mut AudioBuffer| {
        (!buffer.mData.is_null()
            && buffer.mDataByteSize as usize >= frames * std::mem::size_of::<f32>())
        .then(|| unsafe { std::slice::from_raw_parts_mut(buffer.mData as *mut f32, frames) })
    };
    Some([plane(left)?, plane(right)?])
}

/// Render callback for the output unit — drains the ring buffer into the output device.
unsafe extern "C" fn route_render_callback(
    in_ref_con: *mut std::ffi::c_void,
    _io_action_flags: *mut AudioUnitRenderActionFlags,
    _in_time_stamp: *const AudioTimeStamp,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    if io_data.is_null() {
        return 0;
    }

    let now = unsafe { AudioGetCurrentHostTime() };
    let state = unsafe { &mut *(in_ref_con as *mut OutputCallbackState) };
    let buffers = unsafe { abl_buffers_mut(io_data) };
    let n_buffers = buffers.len();
    let ring_frames = state.ring.available();

    let Some(mut out) = output_planes(buffers, in_number_frames as usize) else {
        for buffer in unsafe { abl_buffers_mut(io_data) } {
            if !buffer.mData.is_null() {
                unsafe { std::ptr::write_bytes(buffer.mData.cast::<u8>(), 0, buffer.mDataByteSize as usize) };
            }
        }
        return 0;
    };

    state.playout.render(&state.ring, &mut out, now);

    let vol = f32::from_bits(state.volume.load(Ordering::Relaxed));
    for plane in out.iter_mut() {
        mixer::apply_volume(plane, vol);
    }

    if !state.logged_once {
        state.logged_once = true;
        tracing::info!(
            n_buffers,
            in_number_frames,
            ring_frames,
            peak_abs_f32 = out[0].iter().fold(0.0f32, |p, s| p.max(s.abs())),
            "render callback first fire"
        );
    }

    0
}

// ── Route setup ────────────────────────────────────────────────────────────────

/// Claude: the layout every route hands its output unit: non-interleaved stereo f32.
fn route_format(sample_rate: f64) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: sample_rate,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat
            | kAudioFormatFlagIsPacked
            | kAudioFormatFlagIsNonInterleaved,
        mBytesPerPacket: 4,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4,
        mChannelsPerFrame: CHANNELS as u32,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

unsafe fn attach_route(
    capture: &mut TapCapture,
    dest_device: AudioObjectID,
    volume: Arc<AtomicU32>,
) -> Result<Playback, String> {
    let mut output = unsafe { OutputUnit::new(dest_device) }?;

    let device_format = unsafe { output.device_format() }?;
    tracing::info!(
        sample_rate = device_format.mSampleRate,
        channels_per_frame = device_format.mChannelsPerFrame,
        format_flags = device_format.mFormatFlags,
        is_non_interleaved = (device_format.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0,
        "destination device format (output unit output scope)"
    );

    let dst_asbd = route_format(device_format.mSampleRate);
    tracing::info!(
        sample_rate        = dst_asbd.mSampleRate,
        channels_per_frame = dst_asbd.mChannelsPerFrame,
        format_flags       = dst_asbd.mFormatFlags,
        bytes_per_frame    = dst_asbd.mBytesPerFrame,
        "dst_asbd (output unit client format)"
    );
    unsafe { output.set_client_format(&dst_asbd) }?;

    let playout = Playout::new(
        capture.format.mSampleRate,
        dst_asbd.mSampleRate,
        unsafe { AudioGetHostClockFrequency() },
    )?;
    let health = playout.health();
    tracing::info!(
        src_rate = capture.format.mSampleRate,
        dst_rate = dst_asbd.mSampleRate,
        delay_frames = playout.delay_frames(),
        "created resampler"
    );

    let ring = Arc::new(Ring::new());
    unsafe { output.connect(Arc::clone(&ring), playout, volume) }?;

    let sink_slot = unsafe {
        capture.add_sink(Sink {
            ring: Arc::clone(&ring),
            logged_once: false,
        })
    }?;
    if let Err(e) = unsafe { output.start() } {
        capture.remove_sink(sink_slot);
        return Err(e);
    }

    Ok(Playback {
        sink_slot,
        output,
        ring,
        health,
        reported_glitches: [0; 3],
    })
}
