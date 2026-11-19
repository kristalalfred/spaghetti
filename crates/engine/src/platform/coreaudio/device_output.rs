use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use coreaudio_sys::*;
use spaghetti_common::ipc_protocol::FlowStatus;

use crate::abl;
use super::audio;
use crate::flow::{FlowMeter, Link};
use crate::host_time;
use super::microphone;
use crate::mixer::{Cycles, Fader};
use crate::playout::{Playout, PlayoutHealth, Schedule};
use crate::ring::{CHANNELS, Ring};
use crate::sinks::SourceTiming;
use crate::spec::Playback;
use super::stereo_out::StereoOut;

/// Claude: a route's output to a Core Audio device. The device pulls through an
/// IOProc of the route's own, and `Playout` resamples the route's ring to the
/// device rate. With a delay, each frame sounds that many seconds after its
/// capture.
pub struct DeviceOutput {
    io: OutputIo,
    health: Arc<PlayoutHealth>,
    meter: Arc<FlowMeter>,
    cycles: Arc<Cycles>,
    reported_glitches: [u32; 6],
    scheduled: bool,
}

/// Claude: playing `delay` after capture on a device running at `rate`, its
/// own and its first output stream's latency counted, corrected by
/// `latency_offset_ms`.
fn schedule(
    device: AudioObjectID,
    rate: f64,
    delay: f64,
    latency_offset_ms: Option<i32>,
) -> Schedule {
    Schedule {
        delay,
        latency: f64::from(audio::output_latency_frames(device)) / rate
            + f64::from(latency_offset_ms.unwrap_or(0)) / 1000.0,
    }
}

/// Claude: refused as opening a route from `source` to the device would be
/// for its delay, without opening anything.
pub fn check_schedule(
    device: AudioObjectID,
    source: &SourceTiming,
    delay: f64,
    latency_offset_ms: Option<i32>,
) -> Result<(), String> {
    let rate = audio::nominal_rate(device).ok_or("could not read the device's rate")?;
    let schedule = schedule(device, rate, delay, latency_offset_ms);
    Playout::new(source, rate, host_time::ticks_per_second(), Some(schedule)).map(|_| ())
}

impl DeviceOutput {
    /// Claude: and the ring the route's source feeds it through.
    pub unsafe fn open(
        device: AudioObjectID,
        source: SourceTiming,
        volume: Arc<AtomicU32>,
        delay: Option<f64>,
        latency_offset_ms: Option<i32>,
    ) -> Result<(Self, Arc<Ring>), String> {
        microphone::before_opening(device)?;
        let rate = audio::nominal_rate(device).ok_or("could not read the device's rate")?;
        let channels: u32 = audio::stream_configuration(device, kAudioObjectPropertyScopeOutput)
            .unwrap_or_default()
            .iter()
            .sum();
        if channels == 0 {
            return Err("the device has no output channels".into());
        }

        let schedule = delay.map(|delay| schedule(device, rate, delay, latency_offset_ms));
        if let Some(schedule) = schedule {
            tracing::info!(
                delay_ms = schedule.delay * 1000.0,
                latency_ms = schedule.latency * 1000.0,
                "scheduled playout"
            );
        }
        let playout = Playout::new(&source, rate, host_time::ticks_per_second(), schedule)?;
        let ring = playout.ring();
        let health = playout.health();
        tracing::info!(
            src_rate = source.sample_rate,
            dst_rate = rate,
            delay_frames = playout.delay_frames(),
            "created resampler"
        );

        let meter = Arc::new(FlowMeter::new());
        let cycles = Arc::new(Cycles::new());
        let frames = audio::max_buffer_frames(device).unwrap_or(FALLBACK_BUFFER_FRAMES) as usize;
        let io = unsafe {
            OutputIo::new(
                device,
                OutputState {
                    ring: Arc::clone(&ring),
                    playout,
                    fader: Fader::new(f32::from_bits(volume.load(Ordering::Relaxed)), rate),
                    volume,
                    meter: Arc::clone(&meter),
                    cycles: Arc::clone(&cycles),
                    planes: [vec![0.0; frames], vec![0.0; frames]],
                    logged_once: false,
                },
            )
        }?;
        Ok((
            Self {
                io,
                health,
                meter,
                cycles,
                reported_glitches: [0; 6],
                scheduled: schedule.is_some(),
            },
            ring,
        ))
    }

    /// Claude: `Connected` until the route's ring fills enough to play.
    pub fn status(&self) -> FlowStatus {
        self.meter.status(Link::Connected {
            peer: None,
            packets_lost: 0,
            packets_retransmitted: 0,
        })
    }

    pub fn start(&self) -> Result<(), String> {
        unsafe { self.io.start() }
    }

    /// Claude: the render callback's cycles.
    pub fn health(&self) -> &PlayoutHealth {
        &self.health
    }

    pub fn cycles(&self) -> &Cycles {
        &self.cycles
    }

    pub fn playback(&self) -> Playback {
        self.health.playback(&self.cycles)
    }

    pub fn log_health(&mut self, route_id: u32, ring: &Ring) {
        self.health
            .log(route_id, ring, self.scheduled, &mut self.reported_glitches);
    }
}

/// Claude: buffers sized for more frames than any device asks of an IOProc.
const FALLBACK_BUFFER_FRAMES: u32 = 16384;

struct OutputState {
    ring: Arc<Ring>,
    playout: Playout,
    volume: Arc<AtomicU32>,
    fader: Fader,
    meter: Arc<FlowMeter>,
    cycles: Arc<Cycles>,
    /// Claude: the route's stereo, rendered here before it is laid out in the
    /// device's buffers.
    planes: [Vec<f32>; CHANNELS],
    logged_once: bool,
}

/// Claude: the route's IOProc on the device, with the device's inputs off for
/// it.
struct OutputIo {
    device: AudioObjectID,
    ioproc: AudioDeviceIOProcID,
    /// Claude: leaked Box handed to the IOProc; freed once it is destroyed.
    state: *mut OutputState,
}

// Claude: the IOProc is driven from one control thread at a time, and its
// state is only touched by the IOProc while it exists.
unsafe impl Send for OutputIo {}

impl OutputIo {
    unsafe fn new(device: AudioObjectID, state: OutputState) -> Result<Self, String> {
        let state = Box::into_raw(Box::new(state));
        let mut ioproc: AudioDeviceIOProcID = None;
        let status = unsafe {
            AudioDeviceCreateIOProcID(device, Some(route_ioproc), state.cast(), &mut ioproc)
        };
        if status != 0 || ioproc.is_none() {
            drop(unsafe { Box::from_raw(state) });
            return Err(format!("AudioDeviceCreateIOProcID failed: {status:#010x}"));
        }
        let io = Self {
            device,
            ioproc,
            state,
        };
        if audio::has_input(device) {
            let status = unsafe { spaghetti_read_last_inputs_only(device, ioproc, 0) };
            if status != 0 {
                tracing::warn!(status, "could not turn off the device's inputs");
            }
        }
        Ok(io)
    }

    unsafe fn start(&self) -> Result<(), String> {
        let status = unsafe { AudioDeviceStart(self.device, self.ioproc) };
        if status != 0 {
            return Err(format!("AudioDeviceStart failed: {status:#010x}"));
        }
        Ok(())
    }
}

impl Drop for OutputIo {
    fn drop(&mut self) {
        unsafe {
            AudioDeviceStop(self.device, self.ioproc);
            AudioDeviceDestroyIOProcID(self.device, self.ioproc);
            drop(Box::from_raw(self.state));
        }
    }
}

unsafe extern "C" {
    unsafe fn spaghetti_read_last_inputs_only(
        device: AudioObjectID,
        ioproc: AudioDeviceIOProcID,
        count: u32,
    ) -> i32;
}

/// Claude: drains the route's ring into the device's first two output
/// channels.
unsafe extern "C" fn route_ioproc(
    _device: AudioObjectID,
    _now: *const AudioTimeStamp,
    _input: *const AudioBufferList,
    _input_time: *const AudioTimeStamp,
    output: *mut AudioBufferList,
    output_time: *const AudioTimeStamp,
    client: *mut std::ffi::c_void,
) -> OSStatus {
    if output.is_null() || client.is_null() {
        return 0;
    }
    let now = host_time::now();
    let state = unsafe { &mut *(client as *mut OutputState) };
    let buffers = unsafe { abl::buffers_mut(output) };
    for buffer in buffers.iter_mut() {
        if !buffer.mData.is_null() {
            unsafe {
                std::ptr::write_bytes(buffer.mData.cast::<u8>(), 0, buffer.mDataByteSize as usize)
            };
        }
    }
    let Some(device) = StereoOut::new(buffers) else {
        return 0;
    };
    let frames = device.frames;
    let [left, right] = &mut state.planes;
    let (Some(left), Some(right)) = (left.get_mut(..frames), right.get_mut(..frames)) else {
        return 0;
    };
    let mut out = [left, right];

    let stamped = unsafe { output_time.as_ref() }
        .filter(|time| time.mFlags & kAudioTimeStampHostTimeValid != 0)
        .map(|time| time.mHostTime);
    let output = stamped.unwrap_or_else(|| {
        state.playout.count_unstamped_render();
        now
    });
    let ring_frames = state.ring.available();
    state.playout.render(&state.ring, &mut out, now, output);
    if state.playout.is_playing() {
        state
            .meter
            .record((frames * CHANNELS * std::mem::size_of::<f32>()) as u64, now);
    }

    let volume = f32::from_bits(state.volume.load(Ordering::Relaxed));
    state.fader.planar(&mut out, volume);
    for (frame, (&left, &right)) in out[0].iter().zip(out[1].iter()).enumerate() {
        unsafe { device.add(frame, left, right) };
    }
    state.cycles.tick(now, frames, state.playout.device_rate());

    if !state.logged_once {
        state.logged_once = true;
        tracing::info!(
            frames,
            ring_frames,
            peak_abs_f32 = out[0].iter().fold(0.0f32, |p, s| p.max(s.abs())),
            "route IOProc first fire"
        );
    }

    0
}
