use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use coreaudio_sys::*;
use spaghetti_common::ipc_protocol::FlowStatus;

use crate::abl;
use crate::audio;
use crate::flow::{FlowMeter, Link};
use crate::mixer::Fader;
use crate::playout::{Playout, PlayoutHealth, Schedule};
use crate::ring::{CHANNELS, Ring};
use crate::sinks::SourceTiming;

/// Claude: a route's output to a Core Audio device. The device pulls through the
/// render callback, and `Playout` resamples the route's ring to the device rate.
/// With a delay, each frame sounds that many seconds after its capture.
pub struct DeviceOutput {
    unit: OutputUnit,
    health: Arc<PlayoutHealth>,
    meter: Arc<FlowMeter>,
    reported_glitches: [u32; 4],
    scheduled: bool,
}

impl DeviceOutput {
    pub unsafe fn open(
        device: AudioObjectID,
        source: SourceTiming,
        ring: Arc<Ring>,
        volume: Arc<AtomicU32>,
        delay: Option<f64>,
    ) -> Result<Self, String> {
        let mut unit = unsafe { OutputUnit::new(device) }?;

        let device_format = unsafe { unit.device_format() }?;
        tracing::info!(
            sample_rate = device_format.mSampleRate,
            channels_per_frame = device_format.mChannelsPerFrame,
            format_flags = device_format.mFormatFlags,
            is_non_interleaved =
                (device_format.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0,
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
        unsafe { unit.set_client_format(&dst_asbd) }?;

        let schedule = delay.map(|delay| Schedule {
            delay,
            latency: f64::from(audio::output_latency_frames(device)) / device_format.mSampleRate,
        });
        if let Some(schedule) = schedule {
            tracing::info!(
                delay_ms = schedule.delay * 1000.0,
                latency_ms = schedule.latency * 1000.0,
                "scheduled playout"
            );
        }
        let playout = Playout::new(
            &source,
            dst_asbd.mSampleRate,
            unsafe { AudioGetHostClockFrequency() },
            schedule,
        )?;
        let health = playout.health();
        tracing::info!(
            src_rate = source.sample_rate,
            dst_rate = dst_asbd.mSampleRate,
            delay_frames = playout.delay_frames(),
            "created resampler"
        );

        let meter = Arc::new(FlowMeter::new());
        unsafe { unit.connect(ring, playout, volume, Arc::clone(&meter)) }?;
        Ok(Self {
            unit,
            health,
            meter,
            reported_glitches: [0; 4],
            scheduled: schedule.is_some(),
        })
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
        unsafe { self.unit.start() }
    }

    pub fn log_health(&mut self, route_id: u32, ring: &Ring) {
        let report = self.health.report();
        let glitches = [
            report.underruns,
            report.reprimes,
            report.resyncs,
            ring.overflows(),
        ];
        let to_ms = |frames: f64| frames * 1000.0 / self.health.source_rate;
        tracing::debug!(
            route_id,
            fill_frames = report.fill_avg_frames,
            fill_ms = to_ms(report.fill_avg_frames as f64),
            target_ms = to_ms(report.target_frames as f64),
            delivery_ms = to_ms(report.delivery_frames),
            correction_ppm = report.correction_ppm,
            late_ms = self
                .scheduled
                .then(|| to_ms(f64::from(report.lateness_frames))),
            underruns = glitches[0],
            reprimes = glitches[1],
            resyncs = glitches[2],
            overflows = glitches[3],
            "route playout health"
        );
        if glitches != self.reported_glitches {
            tracing::info!(
                route_id,
                underruns = glitches[0] - self.reported_glitches[0],
                reprimes = glitches[1] - self.reported_glitches[1],
                resyncs = glitches[2] - self.reported_glitches[2],
                overflows = glitches[3] - self.reported_glitches[3],
                glitch_delivery_ms = to_ms(report.glitch_span_frames as f64),
                glitch_target_ms = to_ms(report.glitch_target_frames as f64),
                "route playout glitched since the last report"
            );
            self.reported_glitches = glitches;
        }
    }
}

struct OutputCallbackState {
    ring: Arc<Ring>,
    playout: Playout,
    volume: Arc<AtomicU32>,
    fader: Fader,
    meter: Arc<FlowMeter>,
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
        meter: Arc<FlowMeter>,
    ) -> Result<(), String> {
        self.render_state = Box::into_raw(Box::new(OutputCallbackState {
            ring,
            playout,
            fader: Fader::new(f32::from_bits(volume.load(Ordering::Relaxed))),
            volume,
            meter,
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
    in_time_stamp: *const AudioTimeStamp,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    if io_data.is_null() {
        return 0;
    }

    let now = unsafe { AudioGetCurrentHostTime() };
    let state = unsafe { &mut *(in_ref_con as *mut OutputCallbackState) };
    let buffers = unsafe { abl::buffers_mut(io_data) };
    let n_buffers = buffers.len();
    let ring_frames = state.ring.available();

    let Some(mut out) = output_planes(buffers, in_number_frames as usize) else {
        for buffer in unsafe { abl::buffers_mut(io_data) } {
            if !buffer.mData.is_null() {
                unsafe { std::ptr::write_bytes(buffer.mData.cast::<u8>(), 0, buffer.mDataByteSize as usize) };
            }
        }
        return 0;
    };

    let output = unsafe { in_time_stamp.as_ref() }
        .filter(|time| time.mFlags & kAudioTimeStampHostTimeValid != 0)
        .map_or(now, |time| time.mHostTime);
    state.playout.render(&state.ring, &mut out, now, output);
    if state.playout.is_playing() {
        state.meter.record(
            (in_number_frames as usize * CHANNELS * std::mem::size_of::<f32>()) as u64,
            now,
        );
    }

    let volume = f32::from_bits(state.volume.load(Ordering::Relaxed));
    state.fader.planar(&mut out, volume);

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
