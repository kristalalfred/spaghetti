use std::collections::HashMap;
use std::ffi::CString;

use coreaudio_sys::AudioObjectID;
use spaghetti_common::ipc_protocol::{AppInfo, TapInfo, TapMode};

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

/// A live process tap with its backing aggregate device.
pub struct Tap {
    pub tap_obj_id: AudioObjectID,
    pub aggregate_device_id: AudioObjectID,
    pub pid: u32,
    pub app_name: String,
    pub mode: TapMode,
}

/// Manages the lifecycle of Core Audio process taps.
pub struct TapManager {
    taps: HashMap<u32, Tap>,
    next_id: u32,
}

impl TapManager {
    pub fn new() -> Self {
        Self {
            taps: HashMap::new(),
            next_id: 1,
        }
    }

    /// Create a process tap on the given PID and a private aggregate device to read from it.
    pub fn create_tap(&mut self, pid: u32, mode: TapMode) -> Result<u32, String> {
        let pid_i32 = pid as i32;
        let mut tap_obj_id: AudioObjectID = 0;
        let mute_behavior = if mode == TapMode::MuteSourceWhenTapped { 1 } else { 0 };

        let status = unsafe { spaghetti_create_tap(&pid_i32, 1, mute_behavior, &mut tap_obj_id) };
        if status != 0 {
            return Err(format!(
                "AudioHardwareCreateProcessTap failed: OSStatus {status}"
            ));
        }

        let agg_uid = format!("spaghetti_tap_agg_{}", self.next_id);
        let agg_uid_c = CString::new(agg_uid)
            .map_err(|e| format!("invalid aggregate UID: {e}"))?;
        let mut agg_id: AudioObjectID = 0;

        let status = unsafe {
            spaghetti_create_aggregate_for_tap(tap_obj_id, agg_uid_c.as_ptr(), &mut agg_id)
        };
        if status != 0 {
            unsafe { spaghetti_destroy_tap(tap_obj_id) };
            return Err(format!(
                "AudioHardwareCreateAggregateDevice failed: OSStatus {status}"
            ));
        }

        let id = self.next_id;
        self.next_id += 1;

        let app_name = get_process_name(pid).unwrap_or_else(|| format!("PID {pid}"));

        self.taps.insert(
            id,
            Tap {
                tap_obj_id,
                aggregate_device_id: agg_id,
                pid,
                app_name,
                mode,
            },
        );

        tracing::info!(tap_id = id, pid, "created process tap");
        Ok(id)
    }

    /// Tear down a tap and its aggregate device.
    pub fn destroy_tap(&mut self, tap_id: u32) -> Result<(), String> {
        let tap = self
            .taps
            .remove(&tap_id)
            .ok_or_else(|| format!("tap {tap_id} not found"))?;

        let status = unsafe { spaghetti_destroy_aggregate(tap.aggregate_device_id) };
        if status != 0 {
            tracing::warn!(tap_id, status, "spaghetti_destroy_aggregate returned non-zero OSStatus");
        }

        let status = unsafe { spaghetti_destroy_tap(tap.tap_obj_id) };
        if status != 0 {
            tracing::warn!(tap_id, status, "spaghetti_destroy_tap returned non-zero OSStatus");
        }

        tracing::info!(tap_id, "destroyed process tap");
        Ok(())
    }

    pub fn list_taps(&self) -> Vec<TapInfo> {
        self.taps
            .iter()
            .map(|(&id, tap)| TapInfo {
                tap_id: id,
                pid: tap.pid,
                app_name: tap.app_name.clone(),
                mode: tap.mode,
            })
            .collect()
    }

    #[allow(dead_code)]
    pub fn get_tap(&self, tap_id: u32) -> Option<&Tap> {
        self.taps.get(&tap_id)
    }

    pub fn tap_count(&self) -> u32 {
        self.taps.len() as u32
    }
}

impl Drop for TapManager {
    fn drop(&mut self) {
        let ids: Vec<u32> = self.taps.keys().copied().collect();
        for id in ids {
            if let Err(e) = self.destroy_tap(id) {
                tracing::warn!(tap_id = id, error = %e, "failed to destroy tap during cleanup");
            }
        }
    }
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
    use coreaudio_sys::*;
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
    use coreaudio_sys::*;
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
