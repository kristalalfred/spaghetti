use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

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

        let thread = std::thread::Builder::new()
            .name(format!("route-{id}"))
            .spawn(move || {
                if let Err(e) = run_audio_route(
                    source_aggregate_device,
                    dest_device_id,
                    route_volume,
                    route_active,
                ) {
                    tracing::error!(route_id = id, "audio route error: {e}");
                }
            })
            .map_err(|e| format!("failed to spawn route thread: {e}"))?;

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
            // Wake the parked thread immediately so it can observe active=false and exit.
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

// -- Audio routing via AUHAL AudioUnits --

/// Configure an AUHAL AudioUnit for input from a device.
///
/// Safety: caller must ensure `device_id` is a valid CoreAudio device.
unsafe fn create_input_unit(device_id: AudioObjectID) -> Result<AudioUnit, String> {
    unsafe {
        let desc = AudioComponentDescription {
            componentType: kAudioUnitType_Output,
            componentSubType: kAudioUnitSubType_HALOutput,
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0,
        };

        let comp = AudioComponentFindNext(std::ptr::null_mut(), &desc);
        if comp.is_null() {
            return Err("no HALOutput component found".into());
        }

        let mut unit: AudioUnit = std::ptr::null_mut();
        let status = AudioComponentInstanceNew(comp, &mut unit);
        if status != 0 {
            return Err(format!("AudioComponentInstanceNew failed: {status}"));
        }

        // Enable input IO (element 1)
        let enable: u32 = 1;
        let status = AudioUnitSetProperty(
            unit,
            kAudioOutputUnitProperty_EnableIO,
            kAudioUnitScope_Input,
            1,
            &enable as *const u32 as *const _,
            std::mem::size_of::<u32>() as u32,
        );
        if status != 0 {
            AudioComponentInstanceDispose(unit);
            return Err(format!("enable input IO failed: {status}"));
        }

        // Disable output IO (element 0) -- capture only
        let disable: u32 = 0;
        let status = AudioUnitSetProperty(
            unit,
            kAudioOutputUnitProperty_EnableIO,
            kAudioUnitScope_Output,
            0,
            &disable as *const u32 as *const _,
            std::mem::size_of::<u32>() as u32,
        );
        if status != 0 {
            AudioComponentInstanceDispose(unit);
            return Err(format!("disable output IO failed: {status}"));
        }

        // Set source device
        let status = AudioUnitSetProperty(
            unit,
            kAudioOutputUnitProperty_CurrentDevice,
            kAudioUnitScope_Global,
            0,
            &device_id as *const AudioObjectID as *const _,
            std::mem::size_of::<AudioObjectID>() as u32,
        );
        if status != 0 {
            AudioComponentInstanceDispose(unit);
            return Err(format!("set input device failed: {status}"));
        }

        Ok(unit)
    }
}

/// Configure an AUHAL AudioUnit for output to a device.
///
/// Safety: caller must ensure `device_id` is a valid CoreAudio device.
unsafe fn create_output_unit(device_id: AudioObjectID) -> Result<AudioUnit, String> {
    unsafe {
        let desc = AudioComponentDescription {
            componentType: kAudioUnitType_Output,
            componentSubType: kAudioUnitSubType_HALOutput,
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0,
        };

        let comp = AudioComponentFindNext(std::ptr::null_mut(), &desc);
        if comp.is_null() {
            return Err("no HALOutput component found".into());
        }

        let mut unit: AudioUnit = std::ptr::null_mut();
        let status = AudioComponentInstanceNew(comp, &mut unit);
        if status != 0 {
            return Err(format!("AudioComponentInstanceNew failed: {status}"));
        }

        // Output is enabled by default on element 0; just set the device
        let status = AudioUnitSetProperty(
            unit,
            kAudioOutputUnitProperty_CurrentDevice,
            kAudioUnitScope_Global,
            0,
            &device_id as *const AudioObjectID as *const _,
            std::mem::size_of::<AudioObjectID>() as u32,
        );
        if status != 0 {
            AudioComponentInstanceDispose(unit);
            return Err(format!("set output device failed: {status}"));
        }

        Ok(unit)
    }
}

/// Core audio routing loop. Runs on a dedicated thread per route.
///
/// Sets up an output AudioUnit on the destination device with a render callback
/// that pulls audio from an input AudioUnit on the source device, applying volume.
fn run_audio_route(
    source_device: AudioObjectID,
    dest_device: AudioObjectID,
    volume: Arc<AtomicU32>,
    active: Arc<AtomicBool>,
) -> Result<(), String> {
    unsafe {
        let input_unit = create_input_unit(source_device)?;
        let output_unit = create_output_unit(dest_device)?;

        // Get the input stream format so we can match it on the output
        let mut format: AudioStreamBasicDescription = std::mem::zeroed();
        let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
        let status = AudioUnitGetProperty(
            input_unit,
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Output, // output scope of input element = what we read
            1,
            &mut format as *mut _ as *mut _,
            &mut size,
        );
        if status != 0 {
            AudioComponentInstanceDispose(input_unit);
            AudioComponentInstanceDispose(output_unit);
            return Err(format!("get input stream format failed: {status}"));
        }

        // Set matching format on output unit's input scope
        let status = AudioUnitSetProperty(
            output_unit,
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Input,
            0,
            &format as *const _ as *const _,
            std::mem::size_of::<AudioStreamBasicDescription>() as u32,
        );
        if status != 0 {
            AudioComponentInstanceDispose(input_unit);
            AudioComponentInstanceDispose(output_unit);
            return Err(format!("set output stream format failed: {status}"));
        }

        // Set up render callback on the output unit that pulls from the input unit
        let callback_state = Box::new(RenderCallbackState {
            input_unit,
            volume: Arc::clone(&volume),
        });
        let callback_state_ptr = Box::into_raw(callback_state);

        let render_callback = AURenderCallbackStruct {
            inputProc: Some(route_render_callback),
            inputProcRefCon: callback_state_ptr as *mut _,
        };

        let status = AudioUnitSetProperty(
            output_unit,
            kAudioUnitProperty_SetRenderCallback,
            kAudioUnitScope_Input,
            0,
            &render_callback as *const _ as *const _,
            std::mem::size_of::<AURenderCallbackStruct>() as u32,
        );
        if status != 0 {
            let _ = Box::from_raw(callback_state_ptr);
            AudioComponentInstanceDispose(input_unit);
            AudioComponentInstanceDispose(output_unit);
            return Err(format!("set render callback failed: {status}"));
        }

        // Initialize and start both units
        let status = AudioUnitInitialize(input_unit);
        if status != 0 {
            let _ = Box::from_raw(callback_state_ptr);
            AudioComponentInstanceDispose(input_unit);
            AudioComponentInstanceDispose(output_unit);
            return Err(format!("initialize input unit failed: {status}"));
        }

        let status = AudioUnitInitialize(output_unit);
        if status != 0 {
            let _ = Box::from_raw(callback_state_ptr);
            AudioUnitUninitialize(input_unit);
            AudioComponentInstanceDispose(input_unit);
            AudioComponentInstanceDispose(output_unit);
            return Err(format!("initialize output unit failed: {status}"));
        }

        let status = AudioOutputUnitStart(input_unit);
        if status != 0 {
            let _ = Box::from_raw(callback_state_ptr);
            AudioUnitUninitialize(input_unit);
            AudioUnitUninitialize(output_unit);
            AudioComponentInstanceDispose(input_unit);
            AudioComponentInstanceDispose(output_unit);
            return Err(format!("start input unit failed: {status}"));
        }

        let status = AudioOutputUnitStart(output_unit);
        if status != 0 {
            let _ = Box::from_raw(callback_state_ptr);
            AudioOutputUnitStop(input_unit);
            AudioUnitUninitialize(input_unit);
            AudioUnitUninitialize(output_unit);
            AudioComponentInstanceDispose(input_unit);
            AudioComponentInstanceDispose(output_unit);
            return Err(format!("start output unit failed: {status}"));
        }

        // Park until destroy_route unparks us and sets active=false.
        // Spurious wakeups are harmless: we re-park if still active.
        while active.load(Ordering::Acquire) {
            std::thread::park();
        }

        // Teardown
        AudioOutputUnitStop(output_unit);
        AudioOutputUnitStop(input_unit);
        AudioUnitUninitialize(output_unit);
        AudioUnitUninitialize(input_unit);
        let _ = Box::from_raw(callback_state_ptr);
        AudioComponentInstanceDispose(output_unit);
        AudioComponentInstanceDispose(input_unit);
    }

    Ok(())
}

struct RenderCallbackState {
    input_unit: AudioUnit,
    volume: Arc<AtomicU32>,
}

// Safety: AudioUnit (a raw pointer) is only accessed from the audio render thread.
unsafe impl Send for RenderCallbackState {}
unsafe impl Sync for RenderCallbackState {}

/// Render callback invoked by the output AudioUnit. Pulls audio from the input
/// unit and applies volume scaling before delivering to the output buffer.
unsafe extern "C" fn route_render_callback(
    in_ref_con: *mut std::ffi::c_void,
    io_action_flags: *mut AudioUnitRenderActionFlags,
    in_time_stamp: *const AudioTimeStamp,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    unsafe {
        let state = &*(in_ref_con as *const RenderCallbackState);

        // Pull audio from the input unit into the output buffers
        let status = AudioUnitRender(
            state.input_unit,
            io_action_flags,
            in_time_stamp,
            1, // input element
            in_number_frames,
            io_data,
        );
        if status != 0 {
            // On failure, zero the buffers to avoid noise
            if !io_data.is_null() {
                let buf_list = &*io_data;
                let buffers = std::slice::from_raw_parts(
                    buf_list.mBuffers.as_ptr(),
                    buf_list.mNumberBuffers as usize,
                );
                for buf in buffers {
                    if !buf.mData.is_null() {
                        std::ptr::write_bytes(
                            buf.mData as *mut u8,
                            0,
                            buf.mDataByteSize as usize,
                        );
                    }
                }
            }
            return 0;
        }

        // Apply volume
        let vol = f32::from_bits(state.volume.load(Ordering::Relaxed));
        if (vol - 1.0).abs() >= f32::EPSILON && !io_data.is_null() {
            let buf_list = &*io_data;
            let buffers = std::slice::from_raw_parts(
                buf_list.mBuffers.as_ptr(),
                buf_list.mNumberBuffers as usize,
            );
            for buf in buffers {
                if !buf.mData.is_null() {
                    let sample_count =
                        buf.mDataByteSize as usize / std::mem::size_of::<f32>();
                    let samples =
                        std::slice::from_raw_parts_mut(buf.mData as *mut f32, sample_count);
                    mixer::apply_volume(samples, vol);
                }
            }
        }

        0
    }
}
