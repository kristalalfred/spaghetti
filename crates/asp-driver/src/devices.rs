//! Per-device state.
//!
//! Every virtual device published by the plugin owns exactly one
//! [`DeviceInstance`]. All mutable state that belongs to a device — ring
//! buffer, volume/mute atomics, IO-running counter, timestamp anchors —
//! lives inside this struct. Plugin-wide state (host interface pointer,
//! activity shm, mach timebase) stays at module level.
//!
//! Instances are constructed once and placed into the registry. IO callbacks
//! read a `&DeviceInstance` via `Arc` clone of the current registry snapshot;
//! no allocation happens on the real-time path.

use std::sync::atomic::{AtomicU32, AtomicU64};

use coreaudio_sys::{
    AudioStreamBasicDescription, kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked,
    kAudioFormatFlagsNativeEndian, kAudioFormatLinearPCM,
};

/// Role each object ID plays within a device.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeviceRole {
    /// The device object itself.
    Device,
    /// Input stream (capture / loopback-read).
    StreamInput,
    /// Output stream (playback / loopback-write).
    StreamOutput,
    /// Output volume control.
    Volume,
    /// Output mute control.
    Mute,
}

/// All state for a single virtual device.
pub struct DeviceInstance {
    // ---- identity ---------------------------------------------------------
    /// Object ID assigned to the device object (unique, monotonic).
    pub device_id: u32,
    /// Object ID of the input stream.
    pub stream_in_id: u32,
    /// Object ID of the output stream.
    pub stream_out_id: u32,
    /// Object ID of the volume control.
    pub volume_id: u32,
    /// Object ID of the mute control.
    pub mute_id: u32,

    /// Stable UID string (e.g. `"SpaghettiDevice_UID"`). Used for
    /// `kAudioDevicePropertyDeviceUID` and reverse-lookup.
    pub uid: String,
    /// Human-readable name (e.g. `"Spaghetti"`).
    pub name: String,

    // ---- format -----------------------------------------------------------
    /// Channels per frame.
    pub channels: u32,
    /// Sample rate in Hz.
    pub sample_rate: f64,

    // ---- audio path -------------------------------------------------------
    /// Interleaved ring buffer, one `f32` (as `AtomicU32` bits) per sample.
    /// Length = `frames * channels`. Boxed at construction; never resized.
    pub ring_buffer: Box<[AtomicU32]>,
    /// Ring buffer length expressed in frames (samples per channel).
    pub ring_frames: usize,

    // ---- controls ---------------------------------------------------------
    /// Output volume scalar stored as `f32::to_bits`. Default: `1.0`.
    pub volume: AtomicU32,
    /// Output mute state (`0` unmuted, `1` muted).
    pub mute: AtomicU32,

    // ---- IO state ---------------------------------------------------------
    /// Active `StartIO` count minus `StopIO` count.
    pub io_running_count: AtomicU32,
    /// Monotonic counter of issued zero-time-stamp anchors.
    pub timestamp_count: AtomicU64,
    /// `mach_absolute_time()` captured when IO last transitioned 0 → 1.
    pub anchor_host_time: AtomicU64,
    /// Mach host ticks per audio frame at this device's sample rate, stored as
    /// `f64::to_bits`. Computed once in `start_io`; zero means uninitialized.
    /// Per-device so devices with different sample rates each get the right
    /// timebase for `get_zero_time_stamp`.
    pub host_ticks_per_frame: AtomicU64,
}

impl DeviceInstance {
    /// Allocate a fresh device. All atomics start at their resting values:
    /// unity volume, unmuted, IO stopped, ring buffer zeroed.
    // A params struct would add noise for a constructor only ever called
    // from `DeviceRegistry::insert_device`; the arg list stays flat.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device_id: u32,
        stream_in_id: u32,
        stream_out_id: u32,
        volume_id: u32,
        mute_id: u32,
        uid: String,
        name: String,
        channels: u32,
        sample_rate: f64,
        ring_frames: usize,
    ) -> Self {
        let len = ring_frames * channels as usize;
        let mut buf = Vec::with_capacity(len);
        buf.resize_with(len, || AtomicU32::new(0));

        Self {
            device_id,
            stream_in_id,
            stream_out_id,
            volume_id,
            mute_id,
            uid,
            name,
            channels,
            sample_rate,
            ring_buffer: buf.into_boxed_slice(),
            ring_frames,
            volume: AtomicU32::new(1.0f32.to_bits()),
            mute: AtomicU32::new(0),
            io_running_count: AtomicU32::new(0),
            timestamp_count: AtomicU64::new(0),
            anchor_host_time: AtomicU64::new(0),
            host_ticks_per_frame: AtomicU64::new(0),
        }
    }

    /// Canonical ASBD for this device.
    pub fn asbd(&self) -> AudioStreamBasicDescription {
        let bytes_per_sample: u32 = 4; // f32
        let bytes_per_frame = bytes_per_sample * self.channels;
        AudioStreamBasicDescription {
            mSampleRate: self.sample_rate,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat
                | kAudioFormatFlagIsPacked
                | kAudioFormatFlagsNativeEndian,
            mBytesPerPacket: bytes_per_frame,
            mFramesPerPacket: 1,
            mBytesPerFrame: bytes_per_frame,
            mChannelsPerFrame: self.channels,
            mBitsPerChannel: bytes_per_sample * 8,
            mReserved: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(channels: u32, sample_rate: f64, ring_frames: usize) -> DeviceInstance {
        DeviceInstance::new(
            100,
            101,
            102,
            103,
            104,
            "test_uid".into(),
            "Test".into(),
            channels,
            sample_rate,
            ring_frames,
        )
    }

    #[test]
    fn asbd_reflects_channels_and_rate() {
        let dev = instance(6, 44_100.0, 1024);
        let asbd = dev.asbd();
        assert_eq!(asbd.mChannelsPerFrame, 6);
        assert_eq!(asbd.mSampleRate, 44_100.0);
        assert_eq!(asbd.mBitsPerChannel, 32);
        assert_eq!(asbd.mBytesPerPacket, 24); // 6 * 4
        assert_eq!(asbd.mBytesPerFrame, 24);
        assert_eq!(asbd.mFramesPerPacket, 1);
    }

    #[test]
    fn asbd_default_stereo() {
        let dev = instance(2, 48_000.0, 16_384);
        let asbd = dev.asbd();
        assert_eq!(asbd.mChannelsPerFrame, 2);
        assert_eq!(asbd.mBytesPerFrame, 8);
        assert_eq!(asbd.mSampleRate, 48_000.0);
    }

    #[test]
    fn ring_buffer_scales_with_channels() {
        let stereo = instance(2, 48_000.0, 16_384);
        assert_eq!(stereo.ring_buffer.len(), 16_384 * 2);

        let six_channel = instance(6, 48_000.0, 16_384);
        assert_eq!(six_channel.ring_buffer.len(), 16_384 * 6);

        let mono = instance(1, 48_000.0, 16_384);
        assert_eq!(mono.ring_buffer.len(), 16_384);
    }
}
