use std::ffi::{CString, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use coreaudio_sys::*;
use spaghetti_common::ipc_protocol::TapMode;

use crate::abl;
use crate::flow::FlowMeter;
use crate::mixer::Fader;
use crate::ring::CHANNELS;
use crate::rt_slots::RtSlots;
use crate::tap::AGGREGATE_UID_PREFIX;

unsafe extern "C" {
    unsafe fn spaghetti_create_tap(
        pids: *const i32,
        pid_count: u32,
        mute_behavior: i32,
        out_tap_id: *mut AudioObjectID,
    ) -> i32;

    unsafe fn spaghetti_destroy_tap(tap_id: AudioObjectID) -> i32;

    unsafe fn spaghetti_retarget_tap(
        tap_id: AudioObjectID,
        pids: *const i32,
        pid_count: u32,
        mute_behavior: i32,
    ) -> i32;

    unsafe fn spaghetti_create_bus_aggregate(
        device_uid: *const i8,
        taps: *const AudioObjectID,
        tap_count: u32,
        aggregate_uid: *const i8,
        out_aggregate_id: *mut AudioObjectID,
    ) -> i32;

    unsafe fn spaghetti_destroy_aggregate(aggregate_id: AudioObjectID) -> i32;

    unsafe fn spaghetti_read_last_inputs_only(
        device: AudioObjectID,
        ioproc: AudioDeviceIOProcID,
        count: u32,
    ) -> i32;
}

pub const MAX_BUS_ROUTES: usize = 64;
const INITIAL_SLOTS: usize = 4;
const IDLE_CHECK: Duration = Duration::from_secs(1);
const QUIET_CHECKS: u32 = 3;

static NEXT_BUS: AtomicU32 = AtomicU32::new(1);

/// Claude: a process tap that captures nothing until a route points it at
/// processes.
struct SlotTap(AudioObjectID);

impl SlotTap {
    fn create() -> Result<Self, String> {
        let mut tap = 0;
        let status = unsafe { spaghetti_create_tap(ptr::null(), 0, 0, &mut tap) };
        if status != 0 {
            return Err(format!(
                "AudioHardwareCreateProcessTap failed: OSStatus {status}"
            ));
        }
        Ok(Self(tap))
    }

    fn retarget(&self, pids: &[u32], mode: TapMode) -> Result<(), String> {
        let pids: Vec<i32> = pids.iter().map(|&pid| pid as i32).collect();
        let mute = i32::from(mode == TapMode::MuteSourceWhenTapped);
        let status =
            unsafe { spaghetti_retarget_tap(self.0, pids.as_ptr(), pids.len() as u32, mute) };
        if status != 0 {
            return Err(format!("retargeting a bus tap failed: OSStatus {status}"));
        }
        Ok(())
    }
}

impl Drop for SlotTap {
    fn drop(&mut self) {
        let status = unsafe { spaghetti_destroy_tap(self.0) };
        if status != 0 {
            tracing::warn!(status, "destroying a bus tap failed");
        }
    }
}

/// Claude: what the IOProc mixes for one attached route.
struct SlotMix {
    slot: usize,
    volume: Arc<AtomicU32>,
    fader: Fader,
    meter: Arc<FlowMeter>,
}

struct BusState {
    mixes: RtSlots<SlotMix, MAX_BUS_ROUTES>,
    /// Claude: the aggregate lists its taps after any inputs of its device, so
    /// slot `n` arrives in input buffer `buffers - slots + n`.
    slots: AtomicUsize,
}

/// Claude: the aggregate device and the IOProc running on it.
struct Running {
    aggregate: AudioObjectID,
    ioproc: AudioDeviceIOProcID,
    _state: Arc<BusState>,
}

impl Running {
    fn start(device_uid: &str, taps: &[SlotTap], state: Arc<BusState>) -> Result<Self, String> {
        let device = CString::new(device_uid).map_err(|e| format!("invalid device UID: {e}"))?;
        let uid = format!(
            "{AGGREGATE_UID_PREFIX}{}_bus_{}",
            std::process::id(),
            NEXT_BUS.fetch_add(1, Ordering::Relaxed)
        );
        let uid = CString::new(uid).expect("the bus UID has no NUL");
        let ids: Vec<AudioObjectID> = taps.iter().map(|tap| tap.0).collect();
        let mut aggregate = 0;
        let status = unsafe {
            spaghetti_create_bus_aggregate(
                device.as_ptr(),
                ids.as_ptr(),
                ids.len() as u32,
                uid.as_ptr(),
                &mut aggregate,
            )
        };
        if status != 0 {
            return Err(format!(
                "creating the bus aggregate failed: OSStatus {status}"
            ));
        }
        state.slots.store(taps.len(), Ordering::Release);

        let mut ioproc: AudioDeviceIOProcID = None;
        let status = unsafe {
            AudioDeviceCreateIOProcID(
                aggregate,
                Some(bus_ioproc),
                Arc::as_ptr(&state).cast_mut().cast(),
                &mut ioproc,
            )
        };
        if status != 0 || ioproc.is_none() {
            unsafe { spaghetti_destroy_aggregate(aggregate) };
            return Err(format!(
                "AudioDeviceCreateIOProcID (bus) failed: {status:#010x}"
            ));
        }
        let running = Self {
            aggregate,
            ioproc,
            _state: state,
        };
        let status =
            unsafe { spaghetti_read_last_inputs_only(aggregate, ioproc, ids.len() as u32) };
        if status != 0 {
            tracing::warn!(status, "could not turn off the bus device's own inputs");
        }
        let status = unsafe { AudioDeviceStart(aggregate, ioproc) };
        if status != 0 {
            return Err(format!("AudioDeviceStart (bus) failed: {status:#010x}"));
        }
        Ok(running)
    }
}

impl Running {
    fn rearm(&self) {
        unsafe {
            AudioDeviceStop(self.aggregate, self.ioproc);
            AudioDeviceStart(self.aggregate, self.ioproc);
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        unsafe {
            AudioDeviceStop(self.aggregate, self.ioproc);
            AudioDeviceDestroyIOProcID(self.aggregate, self.ioproc);
            spaghetti_destroy_aggregate(self.aggregate);
        }
    }
}

/// Claude: what a bus shares with the thread that idles it.
struct Watched {
    running: Mutex<Option<Running>>,
    /// Claude: the processes each slot taps, none for a free slot.
    pids: Mutex<Vec<Vec<u32>>>,
    stop: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Claude: taps routed to one output device, mixed straight into it. The taps
/// and the device share a private aggregate device whose one IOProc reads
/// every tap and writes the device, so the mix runs on the device's clock with
/// no ring or resampler in between, and the aggregate drift-compensates apps
/// that play on another clock. Changing an aggregate's taps while it runs
/// drops audio for all of them, so the bus keeps a fixed set of tap slots and
/// points idle ones at no process.
///
/// A started aggregate waits for a tapped app to play before it runs, but
/// keeps running after the apps stop, so a watcher restarts it once none of
/// them has played for a while, to let the device idle again.
pub struct DeviceBus {
    watched: Arc<Watched>,
    watcher: Option<JoinHandle<()>>,
    taps: Vec<SlotTap>,
    /// Claude: for each slot, the index of its mix while a route is attached.
    attached: Vec<Option<usize>>,
    state: Arc<BusState>,
    device_uid: String,
}

impl DeviceBus {
    pub fn open(device_uid: &str) -> Result<Self, String> {
        let taps = (0..INITIAL_SLOTS)
            .map(|_| SlotTap::create())
            .collect::<Result<Vec<_>, _>>()?;
        let state = Arc::new(BusState {
            mixes: RtSlots::new(),
            slots: AtomicUsize::new(0),
        });
        let running = Running::start(device_uid, &taps, Arc::clone(&state))?;
        let watched = Arc::new(Watched {
            running: Mutex::new(Some(running)),
            pids: Mutex::new(vec![Vec::new(); taps.len()]),
            stop: AtomicBool::new(false),
        });
        let watcher = {
            let watched = Arc::clone(&watched);
            let device_uid = device_uid.to_string();
            thread::Builder::new()
                .name("spaghetti-bus".into())
                .spawn(move || watch(&watched, &device_uid))
                .map_err(|e| format!("starting the bus watcher failed: {e}"))?
        };
        tracing::info!(device_uid, slots = taps.len(), "opened device bus");
        Ok(Self {
            watched,
            watcher: Some(watcher),
            attached: vec![None; taps.len()],
            taps,
            state,
            device_uid: device_uid.to_string(),
        })
    }

    /// Claude: mixes the processes into the device at the route's volume from
    /// now on, returning the slot that does it.
    pub fn attach(
        &mut self,
        pids: &[u32],
        mode: TapMode,
        volume: Arc<AtomicU32>,
        meter: Arc<FlowMeter>,
    ) -> Result<usize, String> {
        if lock(&self.watched.running).is_none() {
            self.restart()?;
        }
        let slot = match self.free_slot() {
            Some(slot) => slot,
            None => {
                self.grow()?;
                self.free_slot().expect("growing adds free slots")
            }
        };
        let gain = f32::from_bits(volume.load(Ordering::Relaxed));
        let mix = SlotMix {
            slot,
            volume,
            fader: Fader::new(gain),
            meter,
        };
        let index = self
            .state
            .mixes
            .insert(Box::new(mix))
            .map_err(|_| format!("a bus mixes at most {MAX_BUS_ROUTES} routes"))?;
        if let Err(e) = self.taps[slot].retarget(pids, mode) {
            drop(self.state.mixes.remove(index));
            return Err(e);
        }
        self.attached[slot] = Some(index);
        lock(&self.watched.pids)[slot] = pids.to_vec();
        Ok(slot)
    }

    pub fn retarget(&self, slot: usize, pids: &[u32], mode: TapMode) -> Result<(), String> {
        self.taps[slot].retarget(pids, mode)?;
        lock(&self.watched.pids)[slot] = pids.to_vec();
        Ok(())
    }

    pub fn detach(&mut self, slot: usize) {
        if let Err(e) = self.taps[slot].retarget(&[], TapMode::Mirror) {
            tracing::warn!(device_uid = self.device_uid, slot, "{e}");
        }
        lock(&self.watched.pids)[slot].clear();
        if let Some(index) = self.attached[slot].take() {
            drop(self.state.mixes.remove(index));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.attached.iter().all(Option::is_none)
    }

    fn free_slot(&self) -> Option<usize> {
        self.attached.iter().position(Option::is_none)
    }

    fn restart(&self) -> Result<(), String> {
        let mut running = lock(&self.watched.running);
        *running = None;
        *running = Some(Running::start(
            &self.device_uid,
            &self.taps,
            Arc::clone(&self.state),
        )?);
        Ok(())
    }

    /// Claude: adding taps interrupts the aggregate's IO anyway, so the bus
    /// doubles its slots in one rebuild.
    fn grow(&mut self) -> Result<(), String> {
        let count = (self.taps.len() * 2).min(MAX_BUS_ROUTES);
        if count == self.taps.len() {
            return Err(format!("a bus mixes at most {MAX_BUS_ROUTES} routes"));
        }
        let mut added = (self.taps.len()..count)
            .map(|_| SlotTap::create())
            .collect::<Result<Vec<_>, _>>()?;
        *lock(&self.watched.running) = None;
        self.taps.append(&mut added);
        self.attached.resize(count, None);
        lock(&self.watched.pids).resize(count, Vec::new());
        self.restart()?;
        tracing::info!(
            device_uid = self.device_uid,
            slots = count,
            "grew device bus"
        );
        Ok(())
    }
}

impl Drop for DeviceBus {
    fn drop(&mut self) {
        self.watched.stop.store(true, Ordering::Relaxed);
        if let Some(watcher) = self.watcher.take() {
            watcher.thread().unpark();
            let _ = watcher.join();
        }
        drop(lock(&self.watched.running).take());
    }
}

fn watch(watched: &Watched, device_uid: &str) {
    let mut quiet = 0;
    loop {
        thread::park_timeout(IDLE_CHECK);
        if watched.stop.load(Ordering::Relaxed) {
            return;
        }
        let playing = lock(&watched.pids)
            .iter()
            .flatten()
            .any(|&pid| is_running_output(pid));
        quiet = if playing { 0 } else { quiet + 1 };
        if quiet == QUIET_CHECKS
            && let Some(running) = lock(&watched.running).as_ref()
        {
            running.rearm();
            tracing::debug!(device_uid, "bus waits for an app to play");
        }
    }
}

fn is_running_output(pid: u32) -> bool {
    let address = |selector| AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let pid = pid as i32;
    let mut process: AudioObjectID = kAudioObjectUnknown;
    let mut size = std::mem::size_of::<AudioObjectID>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            &address(kAudioHardwarePropertyTranslatePIDToProcessObject),
            std::mem::size_of::<i32>() as u32,
            (&pid as *const i32).cast(),
            &mut size,
            (&mut process as *mut AudioObjectID).cast(),
        )
    };
    if status != 0 || process == kAudioObjectUnknown {
        return false;
    }
    let mut running: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            process,
            &address(kAudioProcessPropertyIsRunningOutput),
            0,
            ptr::null(),
            &mut size,
            (&mut running as *mut u32).cast(),
        )
    };
    status == 0 && running != 0
}

/// Claude: where the device's first two output channels live in its buffers,
/// which may interleave them in one buffer or hold one each. A mono device
/// gets both channels averaged.
struct StereoOut {
    left: Channel,
    right: Channel,
    frames: usize,
}

#[derive(Clone, Copy)]
struct Channel {
    data: *mut f32,
    stride: usize,
}

impl StereoOut {
    fn new(buffers: &[AudioBuffer]) -> Option<Self> {
        let mut channels = buffers
            .iter()
            .filter(|buffer| !buffer.mData.is_null() && buffer.mNumberChannels > 0)
            .flat_map(|buffer| {
                let stride = buffer.mNumberChannels as usize;
                let data = buffer.mData as *mut f32;
                let frames = buffer.mDataByteSize as usize / (stride * std::mem::size_of::<f32>());
                (0..stride).map(move |channel| {
                    let data = unsafe { data.add(channel) };
                    (Channel { data, stride }, frames)
                })
            });
        let (left, left_frames) = channels.next()?;
        let (right, right_frames) = channels.next().unwrap_or((left, left_frames));
        Some(Self {
            left,
            right,
            frames: left_frames.min(right_frames),
        })
    }

    /// # Safety
    ///
    /// Claude: `frame` must be below `self.frames`.
    unsafe fn add(&self, frame: usize, left: f32, right: f32) {
        unsafe {
            if self.left.data == self.right.data {
                *self.left.data.add(frame * self.left.stride) += (left + right) * 0.5;
            } else {
                *self.left.data.add(frame * self.left.stride) += left;
                *self.right.data.add(frame * self.right.stride) += right;
            }
        }
    }

    fn clamp(&self) {
        for channel in [self.left, self.right] {
            for frame in 0..self.frames {
                let sample = unsafe { &mut *channel.data.add(frame * channel.stride) };
                *sample = sample.clamp(-1.0, 1.0);
            }
        }
    }
}

unsafe extern "C" fn bus_ioproc(
    _device: AudioObjectID,
    _now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    _input_time: *const AudioTimeStamp,
    output: *mut AudioBufferList,
    _output_time: *const AudioTimeStamp,
    client: *mut c_void,
) -> OSStatus {
    if output.is_null() || client.is_null() {
        return 0;
    }
    let state = unsafe { &*(client as *const BusState) };
    let outs = unsafe { abl::buffers_mut(output) };
    for buffer in outs.iter_mut() {
        if !buffer.mData.is_null() {
            unsafe {
                ptr::write_bytes(buffer.mData.cast::<u8>(), 0, buffer.mDataByteSize as usize)
            };
        }
    }
    let Some(out) = StereoOut::new(outs) else {
        return 0;
    };
    let ins = if input.is_null() {
        &[][..]
    } else {
        unsafe { abl::buffers(input) }
    };
    let first_tap = ins
        .len()
        .saturating_sub(state.slots.load(Ordering::Acquire));
    let now = unsafe { AudioGetCurrentHostTime() };
    unsafe {
        state.mixes.for_each(|mix| {
            let Some(buffer) = ins.get(first_tap + mix.slot) else {
                return;
            };
            let Some(samples) = abl::samples(buffer) else {
                return;
            };
            let channels = buffer.mNumberChannels.max(1) as usize;
            let right = channels.min(2) - 1;
            let frames = (samples.len() / channels).min(out.frames);
            let volume = f32::from_bits(mix.volume.load(Ordering::Relaxed));
            for (frame, gain) in mix.fader.ramp(frames, volume).enumerate() {
                let at = frame * channels;
                out.add(frame, samples[at] * gain, samples[at + right] * gain);
            }
            mix.meter
                .record((frames * CHANNELS * std::mem::size_of::<f32>()) as u64, now);
        })
    };
    out.clamp();
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(samples: &mut [f32], channels: u32) -> AudioBuffer {
        AudioBuffer {
            mNumberChannels: channels,
            mDataByteSize: std::mem::size_of_val(samples) as u32,
            mData: samples.as_mut_ptr().cast(),
        }
    }

    #[test]
    fn interleaved_stereo_gets_left_then_right() {
        let mut samples = [0.0f32; 4];
        let out = StereoOut::new(&[buffer(&mut samples, 2)]).unwrap();
        assert_eq!(out.frames, 2);
        unsafe {
            out.add(0, 0.25, -0.25);
            out.add(1, 0.5, -0.5);
            out.add(1, 0.75, -0.75);
        }
        out.clamp();
        assert_eq!(samples, [0.25, -0.25, 1.0, -1.0]);
    }

    #[test]
    fn separate_buffers_each_get_a_channel_and_extra_channels_stay_silent() {
        let mut left = [0.0f32; 2];
        let mut right = [0.0f32; 2];
        let mut rear = [0.0f32; 2];
        let buffers = [
            buffer(&mut left, 1),
            buffer(&mut right, 1),
            buffer(&mut rear, 1),
        ];
        let out = StereoOut::new(&buffers).unwrap();
        unsafe { out.add(1, 0.5, -0.5) };
        assert_eq!((left, right, rear), ([0.0, 0.5], [0.0, -0.5], [0.0, 0.0]));
    }

    #[test]
    fn a_mono_device_gets_the_average() {
        let mut mono = [0.0f32; 1];
        let out = StereoOut::new(&[buffer(&mut mono, 1)]).unwrap();
        unsafe { out.add(0, 0.5, 0.25) };
        assert_eq!(mono, [0.375]);
    }
}
