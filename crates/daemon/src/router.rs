use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use coreaudio_sys::*;
use spaghetti_common::ipc_protocol::RouteInfo;

use crate::mixer;

/// A live audio route: reads from a tap's aggregate device, writes to a virtual device.
struct Route {
    route_id: u32,
    tap_id: u32,
    device_uid: String,
    volume: Arc<AtomicU32>,
    output: OutputUnit,
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
        if self.captures.contains_key(&tap_id) {
            return Err(format!("tap {tap_id} is already routed"));
        }

        let volume = Arc::new(AtomicU32::new(initial_volume.to_bits()));
        let (capture, output) = unsafe {
            start_route(source_aggregate_device, dest_device_id, Arc::clone(&volume))
        }?;

        let id = self.next_id;
        self.next_id += 1;
        self.captures.insert(tap_id, capture);
        self.routes.insert(
            id,
            Route {
                route_id: id,
                tap_id,
                device_uid: dest_device_uid.to_string(),
                volume,
                output,
            },
        );

        tracing::info!(route_id = id, tap_id, dest = dest_device_uid, "created route");
        Ok(id)
    }

    pub fn destroy_route(&mut self, route_id: u32) -> Result<(), String> {
        let Route { tap_id, output, .. } = self
            .routes
            .remove(&route_id)
            .ok_or_else(|| format!("route {route_id} not found"))?;

        drop(output);
        self.captures.remove(&tap_id);

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

// ── Ring buffer ───────────────────────────────────────────────────────────────
//
// SPSC ring buffer for f32 samples. Sized for ~200ms at 48kHz stereo:
//   2 channels × 48000 Hz × 0.2s = 19200 samples.
// Rounded to the next power-of-two (32768) for cheap index masking.

const RING_CAPACITY: usize = 32768;
const RING_MASK: usize = RING_CAPACITY - 1;

struct RingBuffer {
    data: Box<[f32; RING_CAPACITY]>,
    /// Written by the producer (input callback) only.
    head: AtomicUsize,
    /// Written by the consumer (output callback) only.
    tail: AtomicUsize,
}

impl RingBuffer {
    fn new() -> Self {
        Self {
            data: Box::new([0.0f32; RING_CAPACITY]),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Push samples from a non-interleaved `AudioBufferList` into the ring.
    ///
    /// Interleaves channels on the way in so the output reads a flat stream.
    /// Drops the entire block if the ring is full.
    ///
    /// Safety: `buf_list` must point to a valid `AudioBufferList` with `mNumberBuffers`
    /// valid `AudioBuffer` elements.
    unsafe fn push_from_abl(&self, buf_list: *const AudioBufferList, n_frames: u32) {
        let buffers = unsafe { abl_buffers(buf_list) };
        let n_channels = buffers.len();
        if n_channels == 0 {
            return;
        }

        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let free = RING_CAPACITY - head.wrapping_sub(tail);
        let needed = n_frames as usize * n_channels;

        if free < needed {
            tracing::warn!(dropped = needed, "ring buffer full, dropping samples");
            return;
        }

        for frame in 0..n_frames as usize {
            for (ch, buf) in buffers.iter().enumerate() {
                let sample = if buf.mData.is_null() {
                    0.0f32
                } else {
                    let samples = unsafe {
                        std::slice::from_raw_parts(
                            buf.mData as *const f32,
                            buf.mDataByteSize as usize / std::mem::size_of::<f32>(),
                        )
                    };
                    if frame < samples.len() { samples[frame] } else { 0.0 }
                };
                let idx = head.wrapping_add(frame * n_channels + ch) & RING_MASK;
                // Safety: only the producer writes between head and head+needed.
                unsafe {
                    let slot = self.data.as_ptr().add(idx) as *mut f32;
                    slot.write(sample);
                }
            }
        }

        self.head.store(head.wrapping_add(needed), Ordering::Release);
    }

    /// Pop samples from the ring into a non-interleaved `AudioBufferList`.
    ///
    /// Zero-fills frames for which no data is available. Returns the number of
    /// frames actually read from the ring (may be less than `n_frames` if the
    /// ring is starved).
    ///
    /// Safety: `buf_list` must point to a valid `AudioBufferList`.
    unsafe fn pop_to_abl(&self, buf_list: *mut AudioBufferList, n_frames: u32) -> u32 {
        let buffers = unsafe { abl_buffers_mut(buf_list) };
        let n_channels = buffers.len();
        if n_channels == 0 {
            return 0;
        }

        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let available = head.wrapping_sub(tail);
        let needed = n_frames as usize * n_channels;
        let can_read = available.min(needed);
        let frames_avail = can_read / n_channels;

        for frame in 0..n_frames as usize {
            for (ch, buf) in buffers.iter_mut().enumerate() {
                if buf.mData.is_null() {
                    continue;
                }
                let samples = unsafe {
                    std::slice::from_raw_parts_mut(
                        buf.mData as *mut f32,
                        buf.mDataByteSize as usize / std::mem::size_of::<f32>(),
                    )
                };
                if frame >= samples.len() {
                    continue;
                }
                samples[frame] = if frame < frames_avail {
                    let idx = tail.wrapping_add(frame * n_channels + ch) & RING_MASK;
                    self.data[idx]
                } else {
                    0.0
                };
            }
        }

        self.tail.store(tail.wrapping_add(can_read), Ordering::Release);
        frames_avail as u32
    }
}

// Safety: SPSC — head written by one callback thread, tail by the other.
unsafe impl Send for RingBuffer {}
unsafe impl Sync for RingBuffer {}

// ── Capture side: one IOProc per tap ──────────────────────────────────────────

/// Query the tap aggregate device's input stream format.
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

/// Owns the single IOProc registered on a tap's aggregate device.
///
/// Capture uses `AudioDeviceCreateIOProcID` + `AudioDeviceStart` directly on the
/// tap aggregate device. This is the pattern Apple's `AudioCap` sample uses for
/// process taps and avoids `-10863 kAudioUnitErr_CannotDoInCurrentContext` from
/// pulling tap frames through an AUHAL input unit.
struct TapCapture {
    device: AudioObjectID,
    ioproc_id: AudioDeviceIOProcID,
    sink: *mut Sink,
}

// Safety: the sink is only dereferenced by the IOProc while it is registered.
unsafe impl Send for TapCapture {}

impl TapCapture {
    unsafe fn start(device: AudioObjectID, sink: Sink) -> Result<Self, String> {
        let sink = Box::into_raw(Box::new(sink));
        let mut ioproc_id: AudioDeviceIOProcID = None;
        let status = unsafe {
            AudioDeviceCreateIOProcID(device, Some(capture_ioproc), sink.cast(), &mut ioproc_id)
        };
        if status != 0 || ioproc_id.is_none() {
            drop(unsafe { Box::from_raw(sink) });
            return Err(format!("AudioDeviceCreateIOProcID failed: {status:#010x}"));
        }

        let capture = Self {
            device,
            ioproc_id,
            sink,
        };
        let status = unsafe { AudioDeviceStart(device, ioproc_id) };
        if status != 0 {
            return Err(format!("AudioDeviceStart (IOProc) failed: {status:#010x}"));
        }
        Ok(capture)
    }
}

impl Drop for TapCapture {
    fn drop(&mut self) {
        unsafe {
            AudioDeviceStop(self.device, self.ioproc_id);
            AudioDeviceDestroyIOProcID(self.device, self.ioproc_id);
            drop(Box::from_raw(self.sink));
        }
    }
}

/// Pre-allocated converter output capacity per channel. 2048 frames is well
/// above any realistic HAL buffer size (typically 256–1024) and avoids heap
/// allocation in the real-time IOProc.
const CONV_BUF_FRAMES: usize = 2048;

/// One route's share of a tap capture: converts tap frames to the route's
/// destination format and feeds that route's ring buffer.
struct Sink {
    converter: AudioConverterRef,
    ring: Arc<RingBuffer>,
    conv_out_ch0: Vec<f32>,
    conv_out_ch1: Vec<f32>,
    logged_once: bool,
}

impl Sink {
    unsafe fn new(
        src: &AudioStreamBasicDescription,
        dst: &AudioStreamBasicDescription,
        ring: Arc<RingBuffer>,
    ) -> Result<Self, String> {
        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { AudioConverterNew(src, dst, &mut converter) };
        if status != 0 || converter.is_null() {
            return Err(format!("AudioConverterNew failed: {status:#010x}"));
        }
        tracing::info!(
            src_rate = src.mSampleRate,
            dst_rate = dst.mSampleRate,
            src_channels = src.mChannelsPerFrame,
            dst_channels = dst.mChannelsPerFrame,
            "created AudioConverter for SRC"
        );
        Ok(Self {
            converter,
            ring,
            conv_out_ch0: vec![0.0f32; CONV_BUF_FRAMES],
            conv_out_ch1: vec![0.0f32; CONV_BUF_FRAMES],
            logged_once: false,
        })
    }

    /// Run one IOProc buffer through the converter (tap format → destination
    /// format) and push the result into the ring, so the ring and the output
    /// AUHAL see only destination-format frames.
    ///
    /// Safety: `input` must be the valid ABL the HAL handed to the IOProc.
    unsafe fn push(&mut self, input: *const AudioBufferList) {
        let in_buffers = unsafe { abl_buffers(input) };
        let Some(first) = in_buffers.first() else {
            return;
        };

        // Infer frame count from the first buffer. For non-interleaved f32 with
        // mNumberChannels=1, frame_count = mDataByteSize / 4. For interleaved layouts,
        // frame_count = mDataByteSize / (mNumberChannels * 4).
        let channels = first.mNumberChannels.max(1);
        let bytes_per_frame = channels as usize * std::mem::size_of::<f32>();
        let n_in = (first.mDataByteSize as usize) / bytes_per_frame;
        if n_in == 0 {
            return;
        }

        let n_out_capacity = self.conv_out_ch0.len() as u32;
        let mut n_out: u32 = n_out_capacity;

        // Output ABL shape must match `dst_asbd`, which we built as non-interleaved stereo
        // f32 — so always 2 buffers with 1 channel each, regardless of the input ABL layout.
        // (The tap can be interleaved with n_buffers=1; that's fine for input, but the
        // converter rejects a mismatched output shape with -50 paramErr.)
        #[repr(C)]
        struct Abl2 {
            n_buffers: u32,
            buffers: [AudioBuffer; 2],
        }
        let per_buf_bytes = (n_out_capacity as usize * std::mem::size_of::<f32>()) as u32;
        let mut out_abl = Abl2 {
            n_buffers: 2,
            buffers: [
                AudioBuffer {
                    mNumberChannels: 1,
                    mDataByteSize: per_buf_bytes,
                    mData: self.conv_out_ch0.as_mut_ptr().cast(),
                },
                AudioBuffer {
                    mNumberChannels: 1,
                    mDataByteSize: per_buf_bytes,
                    mData: self.conv_out_ch1.as_mut_ptr().cast(),
                },
            ],
        };

        let mut input_ctx = ConverterInputProcState {
            abl: input,
            n_frames: n_in as u32,
            consumed: false,
        };

        // First-call diagnostic: log all relevant parameters BEFORE the converter runs so
        // we see the inputs even when FillComplexBuffer returns an error (e.g. -50 paramErr).
        let is_first = !self.logged_once;
        if is_first {
            let (buf1_ch, buf1_bytes, buf1_null) = match in_buffers.get(1) {
                Some(b) => (b.mNumberChannels, b.mDataByteSize, b.mData.is_null()),
                None => (0, 0, true),
            };
            tracing::info!(
                // Input ABL
                in_n_buffers        = in_buffers.len(),
                in_buf0_channels    = first.mNumberChannels,
                in_buf0_bytes       = first.mDataByteSize,
                in_buf0_null        = first.mData.is_null(),
                in_buf1_channels    = buf1_ch,
                in_buf1_bytes       = buf1_bytes,
                in_buf1_null        = buf1_null,
                n_in,
                // Output ABL
                out_n_buffers       = out_abl.n_buffers,
                out_buf0_channels   = out_abl.buffers[0].mNumberChannels,
                out_buf0_bytes      = out_abl.buffers[0].mDataByteSize,
                out_buf0_null       = out_abl.buffers[0].mData.is_null(),
                out_buf1_channels   = out_abl.buffers[1].mNumberChannels,
                out_buf1_bytes      = out_abl.buffers[1].mDataByteSize,
                out_buf1_null       = out_abl.buffers[1].mData.is_null(),
                n_out_before        = n_out,
                converter_null      = self.converter.is_null(),
                "IOProc first call — pre-converter parameters"
            );
        }

        let conv_status = unsafe {
            AudioConverterFillComplexBuffer(
                self.converter,
                Some(converter_input_proc),
                &mut input_ctx as *mut _ as *mut std::ffi::c_void,
                &mut n_out,
                &mut out_abl as *mut Abl2 as *mut AudioBufferList,
                std::ptr::null_mut(),
            )
        };

        if is_first {
            tracing::info!(
                conv_status,
                n_out_after = n_out,
                "IOProc first call — post-converter result"
            );
            self.logged_once = true;
        }

        if conv_status != 0 {
            tracing::warn!(status = conv_status, "AudioConverterFillComplexBuffer failed");
            return;
        }

        let frames_out = n_out;

        // On the very first successful conversion, log the peak so we know live signal
        // is actually flowing through to the ring.
        if is_first && frames_out > 0 {
            let peak = self.conv_out_ch0[..frames_out as usize]
                .iter()
                .map(|s| s.abs())
                .fold(0.0f32, f32::max);
            tracing::info!(
                n_in,
                frames_out,
                peak_abs_f32 = peak,
                "IOProc first successful conversion"
            );
        }

        unsafe {
            self.ring
                .push_from_abl(&out_abl as *const Abl2 as *const AudioBufferList, frames_out)
        };
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        unsafe { AudioConverterDispose(self.converter) };
    }
}

/// Transient state passed through `AudioConverterFillComplexBuffer`'s input proc.
///
/// The input proc is invoked synchronously inside `FillComplexBuffer`, so this
/// struct lives on the stack of `Sink::push` and is valid for the duration of
/// the call. We use a raw pointer cast to hand it to the C callback.
struct ConverterInputProcState {
    /// Pointer to the ABL the IOProc received from the HAL. Null once consumed.
    abl: *const AudioBufferList,
    /// Number of input frames available.
    n_frames: u32,
    /// Set to true after the input proc has been called once (PCM SRC calls it once).
    consumed: bool,
}

/// Input data proc for `AudioConverterFillComplexBuffer`.
///
/// Called synchronously inside `FillComplexBuffer` to supply the converter with
/// the next batch of input frames. We hand over the single tap ABL we have and
/// mark it consumed so subsequent calls (which shouldn't happen for PCM SRC) return
/// 0 packets.
unsafe extern "C" fn converter_input_proc(
    _in_converter: AudioConverterRef,
    io_number_data_packets: *mut UInt32,
    io_data: *mut AudioBufferList,
    out_data_packet_description: *mut *mut AudioStreamPacketDescription,
    in_user_data: *mut std::ffi::c_void,
) -> OSStatus {
    // `out_data_packet_description` stays null for linear PCM.
    if !out_data_packet_description.is_null() {
        unsafe { *out_data_packet_description = std::ptr::null_mut() };
    }

    let ctx = unsafe { &mut *(in_user_data as *mut ConverterInputProcState) };

    if ctx.consumed || ctx.abl.is_null() {
        // No more input — signal end of stream with 0 packets.
        unsafe { *io_number_data_packets = 0 };
        return 0;
    }

    let src = unsafe { abl_buffers(ctx.abl) };
    let dst = unsafe { abl_buffers_mut(io_data) };
    for (d, s) in dst.iter_mut().zip(src) {
        d.mData = s.mData;
        d.mDataByteSize = s.mDataByteSize;
        d.mNumberChannels = s.mNumberChannels;
    }

    unsafe { *io_number_data_packets = ctx.n_frames };
    ctx.consumed = true;
    0
}

/// Called by the HAL on the tap aggregate device's IO thread. `in_input_data`
/// is the already-captured ABL — no `AudioUnitRender` needed.
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

    let sink = unsafe { &mut *(in_client_data as *mut Sink) };
    unsafe { sink.push(in_input_data) };
    0
}

// ── Output side: one AUHAL per route ──────────────────────────────────────────

/// State for the output render callback.
struct OutputCallbackState {
    ring: Arc<RingBuffer>,
    volume: Arc<AtomicU32>,
    /// Guards the first-fire diagnostic log so it only prints once.
    logged_once: AtomicBool,
}

/// A HAL output unit bound to a destination device, draining a route's ring.
struct OutputUnit {
    unit: AudioUnit,
    /// Leaked Box handed to the render callback; freed after the unit is disposed.
    render_state: *mut OutputCallbackState,
}

// Safety: the unit is driven from one control thread at a time; the render
// state is only read by the render callback while the unit exists.
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

    unsafe fn stream_format(
        &self,
        scope: AudioUnitScope,
    ) -> Result<AudioStreamBasicDescription, OSStatus> {
        let mut format: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
        let status = unsafe {
            AudioUnitGetProperty(
                self.unit,
                kAudioUnitProperty_StreamFormat,
                scope,
                0,
                &mut format as *mut _ as *mut _,
                &mut size,
            )
        };
        if status != 0 {
            return Err(status);
        }
        Ok(format)
    }

    /// Install the render callback that drains `ring`, then initialise the unit.
    unsafe fn connect(&mut self, ring: Arc<RingBuffer>, volume: Arc<AtomicU32>) -> Result<(), String> {
        self.render_state = Box::into_raw(Box::new(OutputCallbackState {
            ring,
            volume,
            logged_once: AtomicBool::new(false),
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

    let state = unsafe { &*(in_ref_con as *const OutputCallbackState) };

    let frames_popped = unsafe { state.ring.pop_to_abl(io_data, in_number_frames) };
    let buffers = unsafe { abl_buffers_mut(io_data) };

    // First-fire diagnostic: log the output ABL layout and ring drain stats.
    if !state.logged_once.swap(true, Ordering::Relaxed) {
        let (n_ch_buf0, bytes_buf0, peak) = match buffers.first() {
            Some(b) => {
                let peak = if b.mData.is_null() {
                    0.0
                } else {
                    let count = b.mDataByteSize as usize / std::mem::size_of::<f32>();
                    let samples = unsafe { std::slice::from_raw_parts(b.mData as *const f32, count) };
                    samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max)
                };
                (b.mNumberChannels, b.mDataByteSize, peak)
            }
            None => (0, 0, 0.0),
        };
        tracing::info!(
            n_buffers = buffers.len(),
            n_channels_buf0 = n_ch_buf0,
            bytes_buf0,
            in_number_frames,
            frames_popped,
            peak_abs_f32 = peak,
            "render callback first fire"
        );
    }

    let vol = f32::from_bits(state.volume.load(Ordering::Relaxed));
    if (vol - 1.0).abs() >= f32::EPSILON {
        for buf in buffers.iter() {
            if !buf.mData.is_null() {
                let sample_count = buf.mDataByteSize as usize / std::mem::size_of::<f32>();
                let samples =
                    unsafe { std::slice::from_raw_parts_mut(buf.mData as *mut f32, sample_count) };
                mixer::apply_volume(samples, vol);
            }
        }
    }

    0
}

// ── Route setup ────────────────────────────────────────────────────────────────

/// Build the converter output format: non-interleaved f32 at the destination's rate.
unsafe fn destination_format(
    output: &OutputUnit,
    format: &AudioStreamBasicDescription,
) -> AudioStreamBasicDescription {
    // Diagnostic: query the output unit's CURRENT input-scope format BEFORE we overwrite it.
    // This reveals what the destination device natively expects.
    let dest_query = unsafe { output.stream_format(kAudioUnitScope_Input) };
    match dest_query {
        Ok(dest_format) => {
            let dest_non_interleaved =
                (dest_format.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0;
            let dest_float = (dest_format.mFormatFlags & kAudioFormatFlagIsFloat) != 0;
            tracing::info!(
                sample_rate = dest_format.mSampleRate,
                channels_per_frame = dest_format.mChannelsPerFrame,
                bytes_per_frame = dest_format.mBytesPerFrame,
                frames_per_packet = dest_format.mFramesPerPacket,
                bits_per_channel = dest_format.mBitsPerChannel,
                format_flags = dest_format.mFormatFlags,
                is_float = dest_float,
                is_non_interleaved = dest_non_interleaved,
                "output unit native format (input scope, before override)"
            );
            if (dest_format.mSampleRate - format.mSampleRate).abs() > 1.0 {
                tracing::warn!(
                    tap_rate = format.mSampleRate,
                    dest_rate = dest_format.mSampleRate,
                    "SAMPLE RATE MISMATCH between tap and destination — this will cause silence or distortion"
                );
            }
            if dest_format.mChannelsPerFrame != format.mChannelsPerFrame {
                tracing::warn!(
                    tap_channels = format.mChannelsPerFrame,
                    dest_channels = dest_format.mChannelsPerFrame,
                    "CHANNEL COUNT MISMATCH between tap and destination"
                );
            }
        }
        Err(status) => {
            tracing::warn!(
                status,
                "could not query output unit's native format (status={:#010x})",
                status
            );
        }
    }

    // Build the destination ASBD as non-interleaved f32 at the device's native rate.
    // We use the queried native format but enforce non-interleaved f32 layout so it
    // matches what our ring buffer and render callback expect.
    let dst_asbd = match dest_query {
        Ok(dest_format) => {
            let mut dst = dest_format;
            dst.mFormatID = kAudioFormatLinearPCM;
            dst.mFormatFlags =
                kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagIsNonInterleaved;
            dst.mBitsPerChannel = 32;
            dst.mBytesPerFrame = 4;
            dst.mFramesPerPacket = 1;
            dst.mBytesPerPacket = 4;
            if dst.mChannelsPerFrame == 0 {
                dst.mChannelsPerFrame = format.mChannelsPerFrame;
            }
            dst
        }
        Err(_) => {
            let mut dst = *format;
            dst.mSampleRate = 44100.0;
            dst
        }
    };

    // Diagnostic: dump dst_asbd in full so we can verify every field before handing
    // it to AudioConverterNew — invalid fields here are the leading cause of -50.
    tracing::info!(
        sample_rate        = dst_asbd.mSampleRate,
        channels_per_frame = dst_asbd.mChannelsPerFrame,
        format_id          = dst_asbd.mFormatID,
        format_flags       = dst_asbd.mFormatFlags,
        bytes_per_packet   = dst_asbd.mBytesPerPacket,
        frames_per_packet  = dst_asbd.mFramesPerPacket,
        bytes_per_frame    = dst_asbd.mBytesPerFrame,
        bits_per_channel   = dst_asbd.mBitsPerChannel,
        is_float           = (dst_asbd.mFormatFlags & kAudioFormatFlagIsFloat) != 0,
        is_non_interleaved = (dst_asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0,
        "dst_asbd (converter output format, built before AudioConverterNew)"
    );

    dst_asbd
}

/// Start capturing `source_device` into a new ring and play that ring on `dest_device`.
///
/// All resources are released on failure; the caller does not need to clean up.
unsafe fn start_route(
    source_device: AudioObjectID,
    dest_device: AudioObjectID,
    volume: Arc<AtomicU32>,
) -> Result<(TapCapture, OutputUnit), String> {
    let format = unsafe { query_device_stream_format(source_device) }?;
    if format.mSampleRate <= 0.0 || format.mBytesPerFrame == 0 {
        return Err(format!(
            "degenerate stream format from aggregate device (rate={}, bpf={})",
            format.mSampleRate, format.mBytesPerFrame
        ));
    }
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

    let mut output = unsafe { OutputUnit::new(dest_device) }?;
    let dst_asbd = unsafe { destination_format(&output, &format) };

    let ring = Arc::new(RingBuffer::new());
    let sink = unsafe { Sink::new(&format, &dst_asbd, Arc::clone(&ring)) }?;
    unsafe { output.connect(ring, volume) }?;

    let capture = unsafe { TapCapture::start(source_device, sink) }?;
    unsafe { output.start() }?;

    Ok((capture, output))
}
