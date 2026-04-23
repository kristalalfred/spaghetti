use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use coreaudio_sys::*;
use spaghetti_common::ipc_protocol::RouteInfo;

use crate::mixer;

/// A live audio route: reads from a tap's aggregate device, writes to a virtual device.
struct Route {
    route_id: u32,
    tap_id: u32,
    device_uid: String,
    volume: Arc<AtomicU32>,
    active: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Manages audio routes between taps and virtual devices.
pub struct Router {
    routes: HashMap<u32, Route>,
    next_id: u32,
}

impl Router {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
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
        let id = self.next_id;
        self.next_id += 1;

        let volume = Arc::new(AtomicU32::new(initial_volume.to_bits()));
        let active = Arc::new(AtomicBool::new(true));

        let route_active = Arc::clone(&active);
        let route_volume = Arc::clone(&volume);

        // The audio thread sends Ok(()) once AUHAL is fully started, or Err(msg) on failure.
        // We block here (with a timeout) so callers get a real success/failure response.
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

        let thread = std::thread::Builder::new()
            .name(format!("route-{id}"))
            .spawn(move || {
                match run_audio_route(
                    source_aggregate_device,
                    dest_device_id,
                    route_volume,
                    route_active,
                    ready_tx,
                ) {
                    Ok(()) => {}
                    Err(e) => tracing::error!(route_id = id, "audio route error: {e}"),
                }
            })
            .map_err(|e| format!("failed to spawn route thread: {e}"))?;

        match ready_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => {}
            Ok(Err(msg)) => {
                let _ = thread.join();
                return Err(msg);
            }
            Err(_) => {
                active.store(false, Ordering::Release);
                thread.thread().unpark();
                let _ = thread.join();
                return Err("audio thread exited before signalling ready".into());
            }
        }

        self.routes.insert(
            id,
            Route {
                route_id: id,
                tap_id,
                device_uid: dest_device_uid.to_string(),
                volume,
                active,
                thread: Some(thread),
            },
        );

        tracing::info!(route_id = id, tap_id, dest = dest_device_uid, "created route");
        Ok(id)
    }

    pub fn destroy_route(&mut self, route_id: u32) -> Result<(), String> {
        let mut route = self
            .routes
            .remove(&route_id)
            .ok_or_else(|| format!("route {route_id} not found"))?;

        route.active.store(false, Ordering::Release);

        if let Some(handle) = route.thread.take() {
            handle.thread().unpark();
            let _ = handle.join();
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
}

impl Drop for Router {
    fn drop(&mut self) {
        let ids: Vec<u32> = self.routes.keys().copied().collect();
        for id in ids {
            let _ = self.destroy_route(id);
        }
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
        let n_channels = unsafe { (*buf_list).mNumberBuffers as usize };
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
            for ch in 0..n_channels {
                let buf = unsafe { &*(*buf_list).mBuffers.as_ptr().add(ch) };
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
        let n_channels = unsafe { (*buf_list).mNumberBuffers as usize };
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
            for ch in 0..n_channels {
                let buf = unsafe { &mut *(*buf_list).mBuffers.as_mut_ptr().add(ch) };
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

// ── Callback state structs ─────────────────────────────────────────────────────

/// Transient state passed through `AudioConverterFillComplexBuffer`'s input proc.
///
/// The input proc is invoked synchronously inside `FillComplexBuffer`, so this
/// struct lives on the stack of `route_ioproc` and is valid for the duration of
/// the call. We use a raw pointer cast to hand it to the C callback.
struct ConverterInputProcState {
    /// Pointer to the ABL the IOProc received from the HAL. Null once consumed.
    abl: *const AudioBufferList,
    /// Number of input frames available.
    n_frames: u32,
    /// Set to true after the input proc has been called once (PCM SRC calls it once).
    consumed: bool,
}

/// State for the Core Audio IOProc capture callback.
///
/// The IOProc is driven directly by the HAL on the tap aggregate device, so there
/// is no `AudioUnit` to pull from — frames arrive through `inInputData`.
#[repr(C)]
struct InputCallbackState {
    ring: Arc<RingBuffer>,
    /// Sample-rate converter: tap rate → destination native rate.
    converter: AudioConverterRef,
    /// Pre-allocated output buffers for the converter (per channel, fixed capacity).
    conv_out_ch0: Vec<f32>,
    conv_out_ch1: Vec<f32>,
    /// Guards the first-fire diagnostic log so it only prints once.
    logged_once: AtomicBool,
}

/// State for the output render callback.
#[repr(C)]
struct OutputCallbackState {
    ring: Arc<RingBuffer>,
    volume: Arc<AtomicU32>,
    /// Guards the first-fire diagnostic log so it only prints once.
    logged_once: AtomicBool,
}

// Safety: each state struct is accessed only from its respective HAL callback thread.
// AudioConverterRef is a raw pointer; we own it exclusively on the IOProc thread.
unsafe impl Send for InputCallbackState {}
unsafe impl Sync for InputCallbackState {}
unsafe impl Send for OutputCallbackState {}
unsafe impl Sync for OutputCallbackState {}

struct AudioSetup {
    source_device: AudioObjectID,
    ioproc_id: AudioDeviceIOProcID,
    output_unit: AudioUnit,
    /// Leaked Box — freed after the IOProc has been stopped and destroyed.
    /// The converter inside is disposed before this Box is freed.
    input_state_ptr: *mut InputCallbackState,
    /// Leaked Box — freed after the output unit has been stopped.
    output_state_ptr: *mut OutputCallbackState,
}

unsafe impl Send for AudioSetup {}

// ── AUHAL unit constructors ────────────────────────────────────────────────────

/// Create a HAL output unit bound to `device_id`.
unsafe fn create_output_unit(device_id: AudioObjectID) -> Result<AudioUnit, String> {
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
        unsafe { AudioComponentInstanceDispose(unit) };
        return Err(format!("set output device failed: {status}"));
    }

    Ok(unit)
}

// ── Route setup ────────────────────────────────────────────────────────────────

/// Query the tap aggregate device's native output stream format.
///
/// This is what the IOProc will deliver in `inInputData` — non-interleaved f32 at
/// the tap's private sample rate, matching the format `AudioCap` reads.
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

/// Initialise the IOProc capture path and the AUHAL output render path, linked by
/// a shared ring buffer.
///
/// Capture uses `AudioDeviceCreateIOProcID` + `AudioDeviceStart` directly on the
/// tap aggregate device. This is the pattern Apple's `AudioCap` sample uses for
/// process taps and avoids `-10863 kAudioUnitErr_CannotDoInCurrentContext` from
/// pulling tap frames through an AUHAL input unit.
///
/// All resources are disposed on failure; the caller does not need to clean up.
unsafe fn setup_audio_route(
    source_device: AudioObjectID,
    dest_device: AudioObjectID,
    volume: Arc<AtomicU32>,
) -> Result<AudioSetup, String> {
    let output_unit = unsafe { create_output_unit(dest_device) }?;

    // Query the format the tap aggregate device produces. The IOProc will deliver
    // non-interleaved f32 buffers in this format.
    let format = match unsafe { query_device_stream_format(source_device) } {
        Ok(f) => f,
        Err(e) => {
            unsafe { AudioComponentInstanceDispose(output_unit) };
            return Err(e);
        }
    };

    if format.mSampleRate <= 0.0 || format.mBytesPerFrame == 0 {
        unsafe { AudioComponentInstanceDispose(output_unit) };
        return Err(format!(
            "degenerate stream format from aggregate device (rate={}, bpf={})",
            format.mSampleRate, format.mBytesPerFrame
        ));
    }

    // Diagnostic: log the tap's ASBD in detail so we can spot format/rate mismatches.
    let is_non_interleaved = (format.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0;
    let is_float = (format.mFormatFlags & kAudioFormatFlagIsFloat) != 0;
    tracing::info!(
        sample_rate = format.mSampleRate,
        channels_per_frame = format.mChannelsPerFrame,
        bytes_per_frame = format.mBytesPerFrame,
        frames_per_packet = format.mFramesPerPacket,
        bits_per_channel = format.mBitsPerChannel,
        format_flags = format.mFormatFlags,
        is_float,
        is_non_interleaved,
        "tap ASBD (source aggregate device input scope)"
    );

    // Diagnostic: query the output unit's CURRENT input-scope format BEFORE we overwrite it.
    // This reveals what the destination device natively expects.
    let mut dest_format: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
    let mut dest_size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    let dest_query_status = unsafe {
        AudioUnitGetProperty(
            output_unit,
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Input,
            0,
            &mut dest_format as *mut _ as *mut _,
            &mut dest_size,
        )
    };
    if dest_query_status == 0 {
        let dest_non_interleaved = (dest_format.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0;
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
    } else {
        tracing::warn!(
            status = dest_query_status,
            "could not query output unit's native format (status={:#010x})",
            dest_query_status
        );
    }

    tracing::debug!(
        sample_rate = format.mSampleRate,
        channels = format.mChannelsPerFrame,
        bpf = format.mBytesPerFrame,
        "tap stream format"
    );

    // Build the destination ASBD as non-interleaved f32 at the device's native rate.
    // We use the queried native format but enforce non-interleaved f32 layout so it
    // matches what our ring buffer and render callback expect.
    let dst_asbd = if dest_query_status == 0 {
        let mut dst = dest_format;
        // Force non-interleaved f32 — our render callback always produces this layout.
        dst.mFormatID = kAudioFormatLinearPCM;
        dst.mFormatFlags = kAudioFormatFlagIsFloat
            | kAudioFormatFlagIsPacked
            | kAudioFormatFlagIsNonInterleaved;
        dst.mBitsPerChannel = 32;
        dst.mBytesPerFrame = 4;
        dst.mFramesPerPacket = 1;
        dst.mBytesPerPacket = 4;
        // Preserve device's channel count; fall back to tap channel count.
        if dst.mChannelsPerFrame == 0 {
            dst.mChannelsPerFrame = format.mChannelsPerFrame;
        }
        dst
    } else {
        // Fall back: same as source but at the assumed destination rate.
        let mut dst = format;
        dst.mSampleRate = 44100.0;
        dst
    };

    // Diagnostic: dump dst_asbd in full so we can verify every field before handing
    // it to AudioConverterNew — invalid fields here are the leading cause of -50.
    {
        let dst_non_interleaved = (dst_asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0;
        let dst_float = (dst_asbd.mFormatFlags & kAudioFormatFlagIsFloat) != 0;
        tracing::info!(
            sample_rate       = dst_asbd.mSampleRate,
            channels_per_frame = dst_asbd.mChannelsPerFrame,
            format_id         = dst_asbd.mFormatID,
            format_flags      = dst_asbd.mFormatFlags,
            bytes_per_packet  = dst_asbd.mBytesPerPacket,
            frames_per_packet = dst_asbd.mFramesPerPacket,
            bytes_per_frame   = dst_asbd.mBytesPerFrame,
            bits_per_channel  = dst_asbd.mBitsPerChannel,
            is_float          = dst_float,
            is_non_interleaved = dst_non_interleaved,
            "dst_asbd (converter output format, built before AudioConverterNew)"
        );
    }

    // Create the sample-rate converter: tap ASBD → destination ASBD.
    let mut converter: AudioConverterRef = std::ptr::null_mut();
    let conv_status = unsafe { AudioConverterNew(&format, &dst_asbd, &mut converter) };
    if conv_status != 0 || converter.is_null() {
        unsafe { AudioComponentInstanceDispose(output_unit) };
        return Err(format!("AudioConverterNew failed: {conv_status:#010x}"));
    }
    tracing::info!(
        src_rate = format.mSampleRate,
        dst_rate = dst_asbd.mSampleRate,
        src_channels = format.mChannelsPerFrame,
        dst_channels = dst_asbd.mChannelsPerFrame,
        "created AudioConverter for SRC"
    );

    // The output AUHAL runs at the device's native rate — no format override needed.
    // The ring buffer now carries destination-rate frames that the device consumes as-is.

    // Shared ring buffer between the IOProc capture and the output render callback.
    let ring = Arc::new(RingBuffer::new());

    // Pre-allocate per-channel output buffers for the converter. 2048 frames is well
    // above any realistic HAL buffer size (typically 256–1024) and avoids heap
    // allocation in the real-time IOProc.
    const CONV_BUF_FRAMES: usize = 2048;

    let input_state = Box::new(InputCallbackState {
        ring: Arc::clone(&ring),
        converter,
        conv_out_ch0: vec![0.0f32; CONV_BUF_FRAMES],
        conv_out_ch1: vec![0.0f32; CONV_BUF_FRAMES],
        logged_once: AtomicBool::new(false),
    });
    let input_state_ptr = Box::into_raw(input_state);

    let output_state = Box::new(OutputCallbackState {
        ring: Arc::clone(&ring),
        volume,
        logged_once: AtomicBool::new(false),
    });
    let output_state_ptr = Box::into_raw(output_state);

    let render_cb = AURenderCallbackStruct {
        inputProc: Some(route_render_callback),
        inputProcRefCon: output_state_ptr.cast(),
    };
    let status = unsafe {
        AudioUnitSetProperty(
            output_unit,
            kAudioUnitProperty_SetRenderCallback,
            kAudioUnitScope_Input,
            0,
            &render_cb as *const _ as *const _,
            std::mem::size_of::<AURenderCallbackStruct>() as u32,
        )
    };
    // Helper macro: dispose converter + free state boxes on error paths.
    // The converter lives inside input_state_ptr; read it out before freeing.
    macro_rules! cleanup_and_err {
        ($extra:expr, $msg:expr) => {{
            unsafe {
                $extra;
                let conv = (*input_state_ptr).converter;
                if !conv.is_null() {
                    AudioConverterDispose(conv);
                }
                let _ = Box::from_raw(output_state_ptr);
                let _ = Box::from_raw(input_state_ptr);
                AudioComponentInstanceDispose(output_unit);
            }
            return Err($msg);
        }};
    }

    if status != 0 {
        cleanup_and_err!((), format!("set render callback failed: {status}"));
    }

    // Register the IOProc on the source aggregate device.
    let mut ioproc_id: AudioDeviceIOProcID = None;
    let status = unsafe {
        AudioDeviceCreateIOProcID(
            source_device,
            Some(route_ioproc),
            input_state_ptr.cast(),
            &mut ioproc_id,
        )
    };
    if status != 0 || ioproc_id.is_none() {
        cleanup_and_err!((), format!("AudioDeviceCreateIOProcID failed: {status:#010x}"));
    }

    let status = unsafe { AudioUnitInitialize(output_unit) };
    if status != 0 {
        cleanup_and_err!(
            AudioDeviceDestroyIOProcID(source_device, ioproc_id),
            format!("initialize output unit failed: {status}")
        );
    }

    let status = unsafe { AudioDeviceStart(source_device, ioproc_id) };
    if status != 0 {
        cleanup_and_err!(
            { AudioUnitUninitialize(output_unit); AudioDeviceDestroyIOProcID(source_device, ioproc_id); },
            format!("AudioDeviceStart (IOProc) failed: {status:#010x}")
        );
    }

    let status = unsafe { AudioOutputUnitStart(output_unit) };
    if status != 0 {
        cleanup_and_err!(
            { AudioDeviceStop(source_device, ioproc_id); AudioDeviceDestroyIOProcID(source_device, ioproc_id); AudioUnitUninitialize(output_unit); },
            format!("start output unit failed: {status}")
        );
    }

    Ok(AudioSetup {
        source_device,
        ioproc_id,
        output_unit,
        input_state_ptr,
        output_state_ptr,
    })
}

// ── Route thread ───────────────────────────────────────────────────────────────

fn run_audio_route(
    source_device: AudioObjectID,
    dest_device: AudioObjectID,
    volume: Arc<AtomicU32>,
    active: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let setup = unsafe { setup_audio_route(source_device, dest_device, volume) };

    match setup {
        Err(e) => {
            let _ = ready_tx.send(Err(e.clone()));
            Err(e)
        }
        Ok(AudioSetup {
            source_device,
            ioproc_id,
            output_unit,
            input_state_ptr,
            output_state_ptr,
        }) => {
            let _ = ready_tx.send(Ok(()));

            // Park until destroy_route signals active=false.
            while active.load(Ordering::Acquire) {
                std::thread::park();
            }

            // Stop the IOProc and the output unit first — no callbacks will fire
            // after Stop returns. Only then is it safe to dispose the converter and
            // drop the state structs.
            // Order matters: Stop → DestroyIOProcID → ConverterDispose → free state.
            unsafe {
                AudioOutputUnitStop(output_unit);
                AudioDeviceStop(source_device, ioproc_id);
                AudioDeviceDestroyIOProcID(source_device, ioproc_id);
                AudioUnitUninitialize(output_unit);
                // Dispose converter before freeing input_state (it owns the ref).
                let conv = (*input_state_ptr).converter;
                if !conv.is_null() {
                    AudioConverterDispose(conv);
                }
                let _ = Box::from_raw(output_state_ptr);
                let _ = Box::from_raw(input_state_ptr);
                AudioComponentInstanceDispose(output_unit);
            }

            Ok(())
        }
    }
}

// ── HAL callbacks ─────────────────────────────────────────────────────────────

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

    // Copy buffer pointers from the HAL ABL into the converter's ABL.
    // The converter owns io_data's structure; we just fill in the data pointers.
    //
    // IMPORTANT: mBuffers is a flexible array member declared as [AudioBuffer; 1].
    // Using .as_ptr().add(i) on that field creates a pointer derived from a
    // single-element array, so i >= 1 is out-of-provenance UB. We go through the
    // struct base address + field offset instead to get valid provenance over the
    // full allocation.
    let src = ctx.abl;
    let n_bufs = unsafe { (*src).mNumberBuffers.min((*io_data).mNumberBuffers) } as usize;
    let src_base = unsafe {
        (src as *const u8)
            .add(std::mem::offset_of!(AudioBufferList, mBuffers))
            .cast::<AudioBuffer>()
    };
    let dst_base = unsafe {
        (io_data as *mut u8)
            .add(std::mem::offset_of!(AudioBufferList, mBuffers))
            .cast::<AudioBuffer>()
    };
    for i in 0..n_bufs {
        let s = unsafe { &*src_base.add(i) };
        let d = unsafe { &mut *dst_base.add(i) };
        d.mData = s.mData;
        d.mDataByteSize = s.mDataByteSize;
        d.mNumberChannels = s.mNumberChannels;
    }

    unsafe { *io_number_data_packets = ctx.n_frames };
    ctx.consumed = true;
    0
}

/// Called by the HAL directly on the tap aggregate device when fresh samples are
/// available. `in_input_data` is the already-captured ABL — no `AudioUnitRender`
/// needed.
///
/// Runs the tap frames through the AudioConverter (SRC: tap rate → dest rate) before
/// pushing them into the ring buffer, so the ring and the output AUHAL see only
/// destination-rate frames.
unsafe extern "C" fn route_ioproc(
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

    let state = unsafe { &mut *(in_client_data as *mut InputCallbackState) };

    let buf_list = unsafe { &*in_input_data };
    let n_buffers = buf_list.mNumberBuffers;
    if n_buffers == 0 {
        return 0;
    }

    // Infer frame count from the first buffer. For non-interleaved f32 with
    // mNumberChannels=1, frame_count = mDataByteSize / 4. For interleaved layouts,
    // frame_count = mDataByteSize / (mNumberChannels * 4).
    let first = unsafe { &*buf_list.mBuffers.as_ptr() };
    let channels = first.mNumberChannels.max(1);
    let bytes_per_frame = channels as usize * std::mem::size_of::<f32>();
    if bytes_per_frame == 0 {
        return 0;
    }
    let n_in = (first.mDataByteSize as usize) / bytes_per_frame;
    if n_in == 0 {
        return 0;
    }

    // Estimate output frame count after SRC. Add a small margin to ensure the
    // converter never truncates the last partial packet.
    let n_out_capacity = state.conv_out_ch0.len() as u32;
    // Compute ceil(n_in * dst_rate / src_rate) via the ratio the converter was built
    // with. As a conservative upper bound we cap at buffer capacity.
    // The converter will report the true count via io_output_data_packet_size.
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
                mData: state.conv_out_ch0.as_mut_ptr().cast(),
            },
            AudioBuffer {
                mNumberChannels: 1,
                mDataByteSize: per_buf_bytes,
                mData: state.conv_out_ch1.as_mut_ptr().cast(),
            },
        ],
    };

    // Input proc context: lives on the stack for the duration of FillComplexBuffer.
    let mut input_ctx = ConverterInputProcState {
        abl: in_input_data,
        n_frames: n_in as u32,
        consumed: false,
    };

    // First-call diagnostic: log all relevant parameters BEFORE the converter runs so
    // we see the inputs even when FillComplexBuffer returns an error (e.g. -50 paramErr).
    let is_first = !state.logged_once.load(Ordering::Relaxed);
    if is_first {
        let (buf1_ch, buf1_bytes, buf1_null) = if n_buffers >= 2 {
            let b = unsafe { &*in_input_data.cast::<u8>()
                .add(std::mem::offset_of!(AudioBufferList, mBuffers))
                .cast::<AudioBuffer>()
                .add(1) };
            (b.mNumberChannels, b.mDataByteSize, b.mData.is_null())
        } else {
            (0, 0, true)
        };
        tracing::info!(
            // Input ABL
            in_n_buffers        = n_buffers,
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
            converter_null      = state.converter.is_null(),
            "IOProc first call — pre-converter parameters"
        );
    }

    let conv_status = unsafe {
        AudioConverterFillComplexBuffer(
            state.converter,
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
        // Mark logged regardless of success so this block fires exactly once.
        state.logged_once.store(true, Ordering::Relaxed);
    }

    if conv_status != 0 {
        // Log and skip — don't crash the real-time thread.
        tracing::warn!(status = conv_status, "AudioConverterFillComplexBuffer failed");
        return 0;
    }

    // n_out now holds the actual number of output frames produced.
    let frames_out = n_out;

    // On the very first successful conversion, log the peak so we know live signal
    // is actually flowing through to the ring.
    if is_first && frames_out > 0 {
        let peak: f32 = {
            let ch0 = &state.conv_out_ch0[..frames_out as usize];
            ch0.iter().map(|s| s.abs()).fold(0.0f32, f32::max)
        };
        tracing::info!(
            n_in,
            frames_out,
            peak_abs_f32 = peak,
            "IOProc first successful conversion"
        );
    }

    unsafe {
        state
            .ring
            .push_from_abl(&out_abl as *const Abl2 as *const AudioBufferList, frames_out)
    };

    0
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

    // First-fire diagnostic: log the output ABL layout and ring drain stats.
    if !state.logged_once.swap(true, Ordering::Relaxed) {
        let buf_list = unsafe { &*io_data };
        let n_buffers = buf_list.mNumberBuffers;
        let (n_ch_buf0, bytes_buf0) = if n_buffers > 0 {
            let b = unsafe { &*buf_list.mBuffers.as_ptr() };
            (b.mNumberChannels, b.mDataByteSize)
        } else {
            (0, 0)
        };
        // Compute peak written to the output to confirm non-silence reaches the device.
        let peak: f32 = if n_buffers > 0 {
            let b = unsafe { &*buf_list.mBuffers.as_ptr() };
            if !b.mData.is_null() {
                let count = b.mDataByteSize as usize / std::mem::size_of::<f32>();
                let samples = unsafe { std::slice::from_raw_parts(b.mData as *const f32, count) };
                samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max)
            } else {
                0.0
            }
        } else {
            0.0
        };
        tracing::info!(
            n_buffers,
            n_channels_buf0 = n_ch_buf0,
            bytes_buf0,
            in_number_frames,
            frames_popped,
            peak_abs_f32 = peak,
            "render callback first fire"
        );
    }

    // Apply volume scaling.
    let vol = f32::from_bits(state.volume.load(Ordering::Relaxed));
    if (vol - 1.0).abs() >= f32::EPSILON {
        let buf_list = unsafe { &*io_data };
        let n_ch = buf_list.mNumberBuffers as usize;
        for ch in 0..n_ch {
            let buf = unsafe { &*buf_list.mBuffers.as_ptr().add(ch) };
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
