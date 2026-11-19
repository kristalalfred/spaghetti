//! Claude: audio sessions: the streams each process plays, grouped per
//! render endpoint, as the system's volume mixer lists them.

use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2, eRender,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::core::Interface;

use super::com;

/// Claude: the processes with a session on any active render endpoint,
/// without the system sounds' and those of sessions that span several
/// processes.
pub fn pids() -> Vec<u32> {
    let Ok(enumerator) = com::enumerator() else {
        return Vec::new();
    };
    let Ok(endpoints) = (unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) })
    else {
        return Vec::new();
    };
    let mut pids = Vec::new();
    for i in 0..unsafe { endpoints.GetCount() }.unwrap_or(0) {
        let Ok(endpoint) = (unsafe { endpoints.Item(i) }) else {
            continue;
        };
        let Ok(manager) = (unsafe { endpoint.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) })
        else {
            continue;
        };
        let Ok(list) = (unsafe { manager.GetSessionEnumerator() }) else {
            continue;
        };
        for j in 0..unsafe { list.GetCount() }.unwrap_or(0) {
            if let Some(pid) = unsafe { list.GetSession(j) }
                .ok()
                .and_then(|control| control.cast::<IAudioSessionControl2>().ok())
                .and_then(|control| pid(&control))
                && !pids.contains(&pid)
            {
                pids.push(pid);
            }
        }
    }
    pids
}

fn pid(control: &IAudioSessionControl2) -> Option<u32> {
    if unsafe { control.IsSystemSoundsSession() } == S_OK {
        return None;
    }
    unsafe { control.GetProcessId() }
        .ok()
        .filter(|&pid| pid != 0)
}
