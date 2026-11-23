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
use crate::host_time;
use crate::mixer::{Cycles, Fader, START_LIMIT};
use crate::ring::CHANNELS;
use crate::rt_slots::RtSlots;
use crate::spec::Tapped;
use super::stereo_out::StereoOut;
use super::audio;
use super::tap::{AGGREGATE_UID_PREFIX, create_tap, destroy_tap, retarget_tap};

unsafe extern "C" {
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

pub const DIRECT: bool = true;
pub const MAX_BUS_ROUTES: usize = 64;
const INITIAL_SLOTS: usize = 4;
const IDLE_CHECK: Duration = Duration::from_secs(1);
const QUIET_CHECKS: u32 = 3;

static NEXT_BUS: AtomicU32 = AtomicU32::new(1);

/// Claude: a process tap that captures nothing until a route points it at
/// processes. Core Audio fixes the device a tap listens to when it creates
/// the tap, so a slot only takes routes that name its device, or none.
struct SlotTap {
    id: AudioObjectID,
    device_uid: Option<String>,
}

impl SlotTap {
    /// Claude: a tap names a device only as a tap of every process on it, so
    /// a slot for a device starts as one and is released at once.
    fn create(device_uid: Option<&str>) -> Result<Self, String> {
        let tapped = match device_uid {
            None => Tapped::Processes(Vec::new()),
            Some(uid) => Tapped::AllBut {
                except: Vec::new(),
                device_uid: Some(uid.to_string()),
            },
        };
        let slot = Self {
            id: create_tap(&tapped, TapMode::Mirror)?,
            device_uid: device_uid.map(str::to_string),
        };
        if device_uid.is_some() {
            slot.release()?;
        }
        Ok(slot)
    }

    fn retarget(&self, tapped: &Tapped, mode: TapMode) -> Result<(), String> {
        if tapped.device_uid() != self.device_uid.as_deref() {
            return Err("a bus slot keeps the device its tap was created for".into());
        }
        retarget_tap(self.id, tapped, mode)
    }

    fn release(&self) -> Result<(), String> {
        retarget_tap(self.id, &Tapped::Processes(Vec::new()), TapMode::Mirror)
    }
}

impl Drop for SlotTap {
    fn drop(&mut self) {
        let status = destroy_tap(self.id);
        if status != 0 {
            tracing::warn!(status, "destroying a bus tap failed");
        }
    }
}

/// Claude: what the IOProc mixes for one attached route.
struct SlotMix {
    slot: usize,
    sample_rate: f64,
    volume: Arc<AtomicU32>,
    fader: Fader,
    meter: Arc<FlowMeter>,
    cycles: Arc<Cycles>,
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
        if let Some(device) = super::audio::find_device_by_uid(device_uid) {
            super::microphone::before_opening(device)?;
        }
        let device = CString::new(device_uid).map_err(|e| format!("invalid device UID: {e}"))?;
        let uid = format!(
            "{AGGREGATE_UID_PREFIX}{}_bus_{}",
            std::process::id(),
            NEXT_BUS.fetch_add(1, Ordering::Relaxed)
        );
        let uid = CString::new(uid).expect("the bus UID has no NUL");
        let ids: Vec<AudioObjectID> = taps.iter().map(|tap| tap.id).collect();
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
    /// Claude: what each slot taps, none for a free slot.
    tapped: Mutex<Vec<Option<Tapped>>>,
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
    sample_rate: f64,
}

impl DeviceBus {
    pub fn open(device_uid: &str) -> Result<Self, String> {
        let taps = (0..INITIAL_SLOTS)
            .map(|_| SlotTap::create(None))
            .collect::<Result<Vec<_>, _>>()?;
        let state = Arc::new(BusState {
            mixes: RtSlots::new(),
            slots: AtomicUsize::new(0),
        });
        let running = Running::start(device_uid, &taps, Arc::clone(&state))?;
        let watched = Arc::new(Watched {
            running: Mutex::new(Some(running)),
            tapped: Mutex::new(vec![None; taps.len()]),
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
        let sample_rate = super::audio::find_device_by_uid(device_uid)
            .and_then(super::audio::nominal_rate)
            .unwrap_or(48000.0);
        Ok(Self {
            watched,
            watcher: Some(watcher),
            attached: vec![None; taps.len()],
            taps,
            state,
            device_uid: device_uid.to_string(),
            sample_rate,
        })
    }

    /// Claude: mixes what `tapped` names into the device at the route's
    /// volume from now on, returning the slot that does it. While those
    /// processes play, it returns once the bus has read the slot, so a tap that
    /// mutes them already does when it returns.
    pub fn attach(
        &mut self,
        tapped: Tapped,
        mode: TapMode,
        volume: Arc<AtomicU32>,
        meter: Arc<FlowMeter>,
        cycles: Arc<Cycles>,
    ) -> Result<usize, String> {
        if lock(&self.watched.running).is_none() {
            self.restart()?;
        }
        let device_uid = tapped.device_uid();
        let slot = match self.free_slot(device_uid) {
            Some(slot) => slot,
            None => {
                self.grow(device_uid)?;
                self.free_slot(device_uid).expect("growing adds a free slot")
            }
        };
        let gain = f32::from_bits(volume.load(Ordering::Relaxed));
        let mix = SlotMix {
            slot,
            sample_rate: self.sample_rate,
            volume,
            fader: Fader::new(gain, self.sample_rate),
            meter,
            cycles: Arc::clone(&cycles),
        };
        let index = self
            .state
            .mixes
            .insert(Box::new(mix))
            .map_err(|_| format!("a bus mixes at most {MAX_BUS_ROUTES} routes"))?;
        if let Err(e) = self.taps[slot].retarget(&tapped, mode) {
            drop(self.state.mixes.remove(index));
            return Err(e);
        }
        self.attached[slot] = Some(index);
        let playing = is_playing(&tapped);
        lock(&self.watched.tapped)[slot] = Some(tapped);
        if playing {
            cycles.wait(2, START_LIMIT);
        }
        Ok(slot)
    }

    pub fn retarget(&self, slot: usize, tapped: &Tapped, mode: TapMode) -> Result<(), String> {
        self.taps[slot].retarget(tapped, mode)?;
        lock(&self.watched.tapped)[slot] = Some(tapped.clone());
        Ok(())
    }

    pub fn detach(&mut self, slot: usize) {
        if let Err(e) = self.taps[slot].release() {
            tracing::warn!(device_uid = self.device_uid, slot, "{e}");
        }
        lock(&self.watched.tapped)[slot] = None;
        if let Some(index) = self.attached[slot].take() {
            drop(self.state.mixes.remove(index));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.attached.iter().all(Option::is_none)
    }

    fn free_slot(&self, device_uid: Option<&str>) -> Option<usize> {
        self.attached
            .iter()
            .zip(&self.taps)
            .position(|(attached, tap)| {
                attached.is_none() && tap.device_uid.as_deref() == device_uid
            })
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
    /// doubles its slots for any device in one rebuild. A slot for one device
    /// is added on its own.
    fn grow(&mut self, device_uid: Option<&str>) -> Result<(), String> {
        let count = match device_uid {
            None => self.taps.len() * 2,
            Some(_) => self.taps.len() + 1,
        }
        .min(MAX_BUS_ROUTES);
        if count == self.taps.len() {
            return Err(format!("a bus mixes at most {MAX_BUS_ROUTES} routes"));
        }
        let mut added = (self.taps.len()..count)
            .map(|_| SlotTap::create(device_uid))
            .collect::<Result<Vec<_>, _>>()?;
        *lock(&self.watched.running) = None;
        self.taps.append(&mut added);
        self.attached.resize(count, None);
        lock(&self.watched.tapped).resize(count, None);
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
        let playing = lock(&watched.tapped).iter().flatten().any(is_playing);
        quiet = if playing { 0 } else { quiet + 1 };
        if quiet == QUIET_CHECKS
            && let Some(running) = lock(&watched.running).as_ref()
        {
            running.rearm();
            tracing::debug!(device_uid, "bus waits for an app to play");
        }
    }
}

/// Claude: whether any process `tapped` names plays now, to its device if it
/// names one.
pub fn is_playing(tapped: &Tapped) -> bool {
    let Tapped::AllBut { except, device_uid } = tapped else {
        return tapped.pids().iter().any(|&pid| is_running_output(pid));
    };
    let device = match device_uid {
        Some(uid) => match audio::find_device_by_uid(uid) {
            Some(device) => Some(device),
            None => return false,
        },
        None => None,
    };
    let own = std::process::id();
    let global = kAudioObjectPropertyScopeGlobal;
    audio::object_list(kAudioObjectSystemObject, kAudioHardwarePropertyProcessObjectList, global)
        .into_iter()
        .filter(|&process| {
            audio::get_u32(process, kAudioProcessPropertyPID, global)
                .is_some_and(|pid| pid != own && !except.contains(&pid))
        })
        .filter(|&process| {
            audio::get_u32(process, kAudioProcessPropertyIsRunningOutput, global)
                .is_some_and(|running| running != 0)
        })
        .any(|process| {
            device.is_none_or(|device| {
                audio::object_list(process, kAudioProcessPropertyDevices, kAudioObjectPropertyScopeOutput)
                    .contains(&device)
            })
        })
}

pub fn is_running_output(pid: u32) -> bool {
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
    let now = host_time::now();
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
            mix.cycles.tick(now, frames, mix.sample_rate);
        })
    };
    out.clamp();
    0
}
