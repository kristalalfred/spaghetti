use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use coreaudio_sys::*;
use spaghetti_common::ipc_protocol::{AppInfo, FlowStatus, TapMode};

use crate::abl;
use crate::flow::{FlowMeter, Link};
use crate::ring::{CHANNELS, Ring};
use crate::sinks::{MAX_SINKS, Sinks, SourceTiming};

unsafe extern "C" {
    unsafe fn spaghetti_create_tap(
        pids: *const i32,
        pid_count: u32,
        mute_behavior: i32,
        out_tap_id: *mut AudioObjectID,
    ) -> i32;

    unsafe fn spaghetti_destroy_tap(tap_id: AudioObjectID) -> i32;

    unsafe fn spaghetti_create_aggregate_for_tap(
        tap_id: AudioObjectID,
        aggregate_uid: *const i8,
        out_aggregate_id: *mut AudioObjectID,
    ) -> i32;

    unsafe fn spaghetti_destroy_aggregate(aggregate_id: AudioObjectID) -> i32;
}

/// Claude: a process tap and the private aggregate device that exposes it.
struct ProcessTap {
    tap: AudioObjectID,
    aggregate: AudioObjectID,
}

impl ProcessTap {
    fn create(pids: &[u32], mode: TapMode, aggregate_uid: &str) -> Result<Self, String> {
        let pids: Vec<i32> = pids.iter().map(|&pid| pid as i32).collect();
        let mut tap: AudioObjectID = 0;
        let mute_behavior = if mode == TapMode::MuteSourceWhenTapped { 1 } else { 0 };

        let status = unsafe {
            spaghetti_create_tap(pids.as_ptr(), pids.len() as u32, mute_behavior, &mut tap)
        };
        if status != 0 {
            return Err(format!(
                "AudioHardwareCreateProcessTap failed: OSStatus {status}"
            ));
        }

        let agg_uid_c = CString::new(aggregate_uid)
            .map_err(|e| format!("invalid aggregate UID: {e}"))?;
        let mut aggregate: AudioObjectID = 0;
        let status =
            unsafe { spaghetti_create_aggregate_for_tap(tap, agg_uid_c.as_ptr(), &mut aggregate) };
        if status != 0 {
            unsafe { spaghetti_destroy_tap(tap) };
            return Err(format!(
                "AudioHardwareCreateAggregateDevice failed: OSStatus {status}"
            ));
        }
        Ok(Self { tap, aggregate })
    }
}

impl Drop for ProcessTap {
    fn drop(&mut self) {
        let status = unsafe { spaghetti_destroy_aggregate(self.aggregate) };
        if status != 0 {
            tracing::warn!(status, "spaghetti_destroy_aggregate returned non-zero OSStatus");
        }
        let status = unsafe { spaghetti_destroy_tap(self.tap) };
        if status != 0 {
            tracing::warn!(status, "spaghetti_destroy_tap returned non-zero OSStatus");
        }
    }
}

/// Claude: what the IOProc reads. It outlives every IOProc that points at it.
struct CaptureState {
    sinks: Sinks,
    meter: FlowMeter,
    logged_once: AtomicBool,
}

/// Claude: a running IOProc on the tap's aggregate device.
struct Capture {
    device: AudioObjectID,
    ioproc_id: AudioDeviceIOProcID,
    _state: Arc<CaptureState>,
}

impl Capture {
    /// Claude: a tap aggregate runs only one IOProc. Starting a second one halts IO
    /// for every IOProc on the device, and removing it does not resume IO, so all
    /// routes on a tap share this IOProc and get their frames through `Sinks`.
    fn start(device: AudioObjectID, state: Arc<CaptureState>) -> Result<Self, String> {
        let mut ioproc_id: AudioDeviceIOProcID = None;
        let status = unsafe {
            AudioDeviceCreateIOProcID(
                device,
                Some(capture_ioproc),
                Arc::as_ptr(&state).cast_mut().cast(),
                &mut ioproc_id,
            )
        };
        if status != 0 || ioproc_id.is_none() {
            return Err(format!("AudioDeviceCreateIOProcID failed: {status:#010x}"));
        }
        let capture = Self {
            device,
            ioproc_id,
            _state: state,
        };
        let status = unsafe { AudioDeviceStart(device, ioproc_id) };
        if status != 0 {
            return Err(format!("AudioDeviceStart (IOProc) failed: {status:#010x}"));
        }
        Ok(capture)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            AudioDeviceStop(self.device, self.ioproc_id);
            AudioDeviceDestroyIOProcID(self.device, self.ioproc_id);
        }
    }
}

/// Claude: an app's audio, captured from a process tap. The IOProc runs only
/// while at least one route is attached.
pub struct TapSource {
    capture: Option<Capture>,
    state: Arc<CaptureState>,
    process_tap: ProcessTap,
    sample_rate: f64,
    pub label: String,
}

impl TapSource {
    pub fn create(source_id: u32, pids: &[u32], mode: TapMode) -> Result<Self, String> {
        let &first = pids.first().ok_or("a tap needs at least one process")?;
        let process_tap = ProcessTap::create(pids, mode, &format!("spaghetti_tap_agg_{source_id}"))?;
        let format = unsafe { query_device_stream_format(process_tap.aggregate) }?;
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

        tracing::info!(source_id, ?pids, "created process tap");
        Ok(Self {
            capture: None,
            state: Arc::new(CaptureState {
                sinks: Sinks::new(),
                meter: FlowMeter::new(),
                logged_once: AtomicBool::new(false),
            }),
            process_tap,
            sample_rate: format.mSampleRate,
            label: get_process_name(first).unwrap_or_else(|| format!("PID {first}")),
        })
    }

    pub fn status(&self) -> FlowStatus {
        let link = if self.capture.is_some() {
            Link::Connected {
                peer: None,
                packets_lost: 0,
                packets_retransmitted: 0,
            }
        } else {
            Link::Idle
        };
        self.state.meter.status(link)
    }

    pub fn timing(&self) -> SourceTiming {
        SourceTiming {
            sample_rate: self.sample_rate,
            delivery_jitter: 0.0,
        }
    }

    pub fn add_sink(&mut self, ring: Arc<Ring>) -> Result<usize, String> {
        let slot = self
            .state
            .sinks
            .insert(Box::new(ring))
            .map_err(|_| format!("a source can feed at most {MAX_SINKS} routes"))?;
        if self.capture.is_none() {
            match Capture::start(self.process_tap.aggregate, Arc::clone(&self.state)) {
                Ok(capture) => self.capture = Some(capture),
                Err(e) => {
                    self.state.sinks.remove(slot);
                    return Err(e);
                }
            }
        }
        Ok(slot)
    }

    pub fn remove_sink(&mut self, slot: usize) {
        drop(self.state.sinks.remove(slot));
        if self.state.sinks.is_empty() && self.capture.take().is_some() {
            tracing::info!(label = self.label, "stopped tap capture");
        }
    }
}

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

/// Claude: `buffers` must be the ABL the HAL handed to the IOProc. Taps deliver
/// interleaved stereo in practice whatever their ASBD says, so both layouts are accepted.
fn push_buffers(ring: &Ring, buffers: &[AudioBuffer], now: u64) -> usize {
    let Some(first) = buffers.first() else {
        return 0;
    };
    if let [left, right, ..] = buffers {
        let frames =
            left.mDataByteSize.min(right.mDataByteSize) as usize / std::mem::size_of::<f32>();
        let planes = unsafe { [abl::samples(left), abl::samples(right)] };
        ring.push_with(frames, now, |f, ch| planes[ch].map_or(0.0, |s| s[f]));
        frames
    } else {
        let channels = first.mNumberChannels.max(1) as usize;
        let frames = first.mDataByteSize as usize / (channels * std::mem::size_of::<f32>());
        let samples = unsafe { abl::samples(first) };
        ring.push_with(frames, now, |f, ch| {
            samples.map_or(0.0, |s| s[f * channels + ch.min(channels - 1)])
        });
        frames
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
    let state = unsafe { &*(in_client_data as *const CaptureState) };
    let buffers = unsafe { abl::buffers(in_input_data) };
    let mut frames = 0;
    unsafe { state.sinks.for_each(|ring| frames = push_buffers(ring, buffers, now)) };
    state
        .meter
        .record((frames * CHANNELS * std::mem::size_of::<f32>()) as u64, now);

    if !state.logged_once.swap(true, Ordering::Relaxed)
        && let Some(first) = buffers.first()
    {
        tracing::info!(
            in_n_buffers = buffers.len(),
            in_buf0_channels = first.mNumberChannels,
            in_buf0_bytes = first.mDataByteSize,
            in_buf0_null = first.mData.is_null(),
            frames,
            "IOProc first call"
        );
    }
    0
}

/// Resolve a PID to its process name via libproc.
fn get_process_name(pid: u32) -> Option<String> {
    let mut buf = [0u8; 1024];
    let len = unsafe { libc::proc_name(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if len > 0 {
        String::from_utf8(buf[..len as usize].to_vec()).ok()
    } else {
        None
    }
}

/// List processes currently producing audio.
///
/// Uses `kAudioHardwarePropertyProcessObjectList` to get process objects known
/// to Core Audio, then queries each for its PID and bundle ID.
pub fn list_audio_apps() -> Vec<AppInfo> {
    use std::{mem, ptr};

    let prop = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyProcessObjectList,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };

    let mut data_size: u32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            kAudioObjectSystemObject,
            &prop,
            0,
            ptr::null(),
            &mut data_size,
        )
    };
    if status != 0 || data_size == 0 {
        return Vec::new();
    }

    let count = data_size as usize / mem::size_of::<AudioObjectID>();
    let mut objects = vec![0u32; count];

    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &prop,
            0,
            ptr::null(),
            &mut data_size,
            objects.as_mut_ptr().cast(),
        )
    };
    if status != 0 {
        return Vec::new();
    }

    let mut apps = Vec::new();
    for &obj in &objects {
        if let Some(info) = process_object_to_app_info(obj) {
            apps.push(info);
        }
    }
    apps
}

/// Extract PID and name from a Core Audio process object.
fn process_object_to_app_info(obj: AudioObjectID) -> Option<AppInfo> {
    use std::{mem, ptr};

    // Query PID
    let pid_prop = AudioObjectPropertyAddress {
        mSelector: kAudioProcessPropertyPID,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };

    let mut pid: i32 = 0;
    let mut size = mem::size_of::<i32>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(obj, &pid_prop, 0, ptr::null(), &mut size, (&mut pid as *mut i32).cast())
    };
    if status != 0 || pid <= 0 {
        return None;
    }

    let name = get_process_name(pid as u32).unwrap_or_else(|| format!("PID {pid}"));

    Some(AppInfo {
        pid: pid as u32,
        name,
    })
}
