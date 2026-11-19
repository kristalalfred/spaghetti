use std::cell::Cell;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};
use windows::core::{HSTRING, w};

use crate::host_time;

/// Claude: joins the calling thread to the multithreaded apartment, once, so
/// it may call the audio interfaces. A thread already in a single-threaded
/// apartment keeps it; the audio interfaces work from either.
pub fn init() {
    thread_local!(static JOINED: Cell<bool> = const { Cell::new(false) });
    JOINED.with(|joined| {
        if !joined.replace(true) {
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        }
    });
}

pub fn enumerator() -> Result<IMMDeviceEnumerator, String> {
    init();
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(|e| format!("open the device enumerator: {e}"))
}

/// Claude: the active endpoint whose ID is `uid`.
pub fn device(uid: &str) -> Result<IMMDevice, String> {
    #[cfg(test)]
    crate::device_lookup::looking_up(uid);
    let device = unsafe { enumerator()?.GetDevice(&HSTRING::from(uid)) }
        .map_err(|_| format!("device '{uid}' not found"))?;
    match unsafe { device.GetState() } {
        Ok(state) if state == DEVICE_STATE_ACTIVE => Ok(device),
        _ => Err(format!("device '{uid}' is not active")),
    }
}

pub fn id(device: &IMMDevice) -> Option<String> {
    unsafe {
        let id = device.GetId().ok()?;
        let text = id.to_string().ok();
        CoTaskMemFree(Some(id.0.cast()));
        text
    }
}

pub fn activate(device: &IMMDevice) -> Result<IAudioClient, String> {
    unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|e| format!("open the device: {e}"))
}

/// Claude: the rate and channels the audio engine mixes a device at, which
/// a shared-mode stream runs at.
#[derive(Debug, Clone, Copy)]
pub struct MixFormat {
    pub rate: u32,
    pub channels: u16,
}

pub fn mix_format(client: &IAudioClient) -> Result<MixFormat, String> {
    unsafe {
        let format = client
            .GetMixFormat()
            .map_err(|e| format!("read the device's format: {e}"))?;
        let mix = MixFormat {
            rate: (*format).nSamplesPerSec,
            channels: (*format).nChannels,
        };
        CoTaskMemFree(Some(format.cast()));
        Ok(mix)
    }
}

/// Claude: every stream the engine opens carries interleaved stereo f32. The
/// audio engine converts it to and from the device's own channels.
pub fn stereo_float(rate: u32) -> WAVEFORMATEX {
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: 2,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * 8,
        nBlockAlign: 8,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

/// Claude: WASAPI stamps positions with the performance counter in 100 ns
/// units, which the host clock counts in its own ticks.
pub fn hns_to_ticks(hns: u64) -> u64 {
    host_time::from_nanos(hns.saturating_mul(100))
}

/// Claude: registers the calling thread with the multimedia scheduler while it lives.
pub struct ProAudio(HANDLE);

impl ProAudio {
    pub fn join() -> Option<Self> {
        let mut task = 0;
        unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task) }
            .ok()
            .map(Self)
    }
}

impl Drop for ProAudio {
    fn drop(&mut self) {
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

/// Claude: an auto-reset event a stream signals when a buffer is ready.
pub struct Event(pub HANDLE);

impl Event {
    pub fn new() -> Result<Self, String> {
        unsafe { CreateEventW(None, false, false, None) }
            .map(Self)
            .map_err(|e| format!("create an event: {e}"))
    }

    /// Claude: whether it was signaled within `millis`.
    pub fn wait(&self, millis: u32) -> bool {
        unsafe { WaitForSingleObject(self.0, millis) == WAIT_OBJECT_0 }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}
