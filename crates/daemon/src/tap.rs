use std::collections::BTreeMap;
use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use core_foundation::bundle::CFBundle;
use core_foundation::string::CFString;
use core_foundation::url::CFURL;
use coreaudio_sys::*;
use spaghetti_common::ipc_protocol::{AppInfo, FlowStatus, TapMode};

use crate::abl;
use crate::delivery::DeliverySpan;
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

    unsafe fn spaghetti_set_tap_processes(
        tap_id: AudioObjectID,
        pids: *const i32,
        pid_count: u32,
    ) -> i32;

    unsafe fn spaghetti_create_aggregate_for_tap(
        tap_id: AudioObjectID,
        aggregate_uid: *const i8,
        out_aggregate_id: *mut AudioObjectID,
    ) -> i32;

    unsafe fn spaghetti_destroy_aggregate(aggregate_id: AudioObjectID) -> i32;
}

pub const AGGREGATE_UID_PREFIX: &str = "spaghetti_tap_agg_";

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
/// A tap's frames land in its rings a buffer at a time as the IOProc captures
/// them, so the IOProc publishes its buffer as the tap's delivery span.
struct CaptureState {
    sinks: Sinks,
    meter: FlowMeter,
    delivery: Arc<DeliverySpan>,
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
        let process_tap =
            ProcessTap::create(pids, mode, &format!("{AGGREGATE_UID_PREFIX}{source_id}"))?;
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
                delivery: Arc::new(DeliverySpan::new(0.0)),
                logged_once: AtomicBool::new(false),
            }),
            process_tap,
            sample_rate: format.mSampleRate,
            label: app_label(first),
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

    /// Claude: replaces the processes the running tap mixes, without stopping
    /// its capture, so an app's helpers can come and go.
    pub fn set_processes(&mut self, pids: &[u32]) -> Result<(), String> {
        let pids: Vec<i32> = pids.iter().map(|&pid| pid as i32).collect();
        let status = unsafe {
            spaghetti_set_tap_processes(self.process_tap.tap, pids.as_ptr(), pids.len() as u32)
        };
        if status != 0 {
            return Err(format!("set the tap's processes failed: OSStatus {status}"));
        }
        tracing::info!(label = self.label, ?pids, "updated tap processes");
        Ok(())
    }

    pub fn timing(&self) -> SourceTiming {
        SourceTiming {
            sample_rate: self.sample_rate,
            delivery: Arc::clone(&self.state.delivery),
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
fn buffer_frames(buffers: &[AudioBuffer]) -> usize {
    match buffers {
        [] => 0,
        [left, right, ..] => {
            left.mDataByteSize.min(right.mDataByteSize) as usize / std::mem::size_of::<f32>()
        }
        [only] => {
            only.mDataByteSize as usize
                / (only.mNumberChannels.max(1) as usize * std::mem::size_of::<f32>())
        }
    }
}

fn push_buffers(ring: &Ring, buffers: &[AudioBuffer], now: u64) {
    let frames = buffer_frames(buffers);
    match buffers {
        [] => {}
        [left, right, ..] => {
            let planes = unsafe { [abl::samples(left), abl::samples(right)] };
            ring.push_with(frames, now, |f, ch| planes[ch].map_or(0.0, |s| s[f]));
        }
        [only] => {
            let channels = only.mNumberChannels.max(1) as usize;
            let samples = unsafe { abl::samples(only) };
            ring.push_with(frames, now, |f, ch| {
                samples.map_or(0.0, |s| s[f * channels + ch.min(channels - 1)])
            });
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
    let state = unsafe { &*(in_client_data as *const CaptureState) };
    let buffers = unsafe { abl::buffers(in_input_data) };
    let frames = buffer_frames(buffers);
    if frames > 0 {
        state.delivery.publish(frames as f64);
    }
    unsafe { state.sinks.for_each(|ring| push_buffers(ring, buffers, now)) };
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

/// Claude: the processes Core Audio knows, grouped under the app bundle each
/// runs from. A helper inside `Slack.app/Contents/Frameworks/...` belongs to
/// Slack; a process outside any `.app` is listed alone.
pub fn list_audio_apps() -> Vec<AppInfo> {
    let mut apps: BTreeMap<String, AppInfo> = BTreeMap::new();
    for pid in audio_process_pids() {
        let executable = process_path(pid);
        match executable.as_deref().and_then(app_bundle) {
            Some(bundle) => apps
                .entry(bundle.to_string())
                .or_insert_with(|| bundle_app(bundle))
                .pids
                .push(pid),
            None => {
                apps.insert(
                    format!("pid:{pid}"),
                    AppInfo {
                        name: get_process_name(pid).unwrap_or_else(|| format!("PID {pid}")),
                        bundle_id: None,
                        bundle_path: None,
                        pids: vec![pid],
                    },
                );
            }
        }
    }
    let mut apps: Vec<AppInfo> = apps.into_values().collect();
    for app in &mut apps {
        app.pids.sort_unstable();
    }
    apps.sort_by_key(|app| app.name.to_lowercase());
    apps
}

fn app_label(pid: u32) -> String {
    process_path(pid)
        .as_deref()
        .and_then(app_bundle)
        .map(|bundle| bundle_app(bundle).name)
        .or_else(|| get_process_name(pid))
        .unwrap_or_else(|| format!("PID {pid}"))
}

/// Claude: the outermost `.app` bundle in an executable's path.
fn app_bundle(executable: &str) -> Option<&str> {
    executable.find(".app/").map(|end| &executable[..end + 4])
}

fn bundle_app(bundle: &str) -> AppInfo {
    let fallback = bundle
        .rsplit('/')
        .next()
        .unwrap_or(bundle)
        .trim_end_matches(".app")
        .to_string();
    let info = CFURL::from_path(bundle, true)
        .and_then(CFBundle::new)
        .map(|bundle| bundle.info_dictionary());
    let string = |key: &str| {
        info.as_ref()?
            .find(CFString::new(key))
            .and_then(|value| value.downcast::<CFString>())
            .map(|value| value.to_string())
            .filter(|value| !value.trim().is_empty())
    };
    AppInfo {
        name: string("CFBundleDisplayName")
            .or_else(|| string("CFBundleName"))
            .unwrap_or(fallback),
        bundle_id: string("CFBundleIdentifier"),
        bundle_path: Some(bundle.to_string()),
        pids: Vec::new(),
    }
}

fn process_path(pid: u32) -> Option<String> {
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let len = unsafe { libc::proc_pidpath(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (len > 0)
        .then(|| String::from_utf8(buf[..len as usize].to_vec()).ok())
        .flatten()
}

fn audio_process_pids() -> Vec<u32> {
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

    let pid_prop = AudioObjectPropertyAddress {
        mSelector: kAudioProcessPropertyPID,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    objects
        .into_iter()
        .filter_map(|obj| {
            let mut pid: i32 = 0;
            let mut size = mem::size_of::<i32>() as u32;
            let status = unsafe {
                AudioObjectGetPropertyData(
                    obj,
                    &pid_prop,
                    0,
                    ptr::null(),
                    &mut size,
                    (&mut pid as *mut i32).cast(),
                )
            };
            (status == 0 && pid > 0).then_some(pid as u32)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_belong_to_the_outermost_app_bundle() {
        assert_eq!(
            app_bundle("/System/Applications/Music.app/Contents/MacOS/Music"),
            Some("/System/Applications/Music.app")
        );
        assert_eq!(
            app_bundle(
                "/Applications/Slack.app/Contents/Frameworks/Slack Helper.app/Contents/MacOS/Slack Helper"
            ),
            Some("/Applications/Slack.app")
        );
        assert_eq!(app_bundle("/usr/sbin/coreaudiod"), None);
        assert_eq!(app_bundle("/Applications/Weird.application/bin/x"), None);
    }

    #[test]
    fn a_bundle_is_named_from_its_info_plist() {
        let music = bundle_app("/System/Applications/Music.app");
        assert_eq!(music.name, "Music");
        assert_eq!(music.bundle_id.as_deref(), Some("com.apple.Music"));
        let missing = bundle_app("/nonexistent/Some Thing.app");
        assert_eq!(missing.name, "Some Thing");
        assert_eq!(missing.bundle_id, None);
    }
}
