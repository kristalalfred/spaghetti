//! Claude: a Core Audio input device as a source. The device's own IOProc
//! captures its first input stream while a route or another consumer takes
//! the frames, and dates each buffer by when its first frame reached the
//! device:
//!
//! ```text
//! captured = inInputTime.mHostTime - (device input latency + stream latency) / rate
//! ```
//!
//! Claude: `AudioHardware.h` (`AudioDeviceIOProc`) defines `inInputTime` as
//! "the time at which the first frame in the data was acquired from the
//! hardware", and `AudioHardwareBase.h` defines `kAudioDevicePropertyLatency`
//! and `kAudioStreamPropertyLatency` as frames of latency in the device and
//! its stream, summed for the stream's total, so the sound reached the device
//! that much before it was acquired. `kAudioDevicePropertySafetyOffset` is
//! "the number of frames ... behind (for input) the current hardware position
//! that is safe to do IO": it bounds when the HAL may read a frame after it
//! was acquired, which `inInputTime` already places, so it is not subtracted.
//! `inNow` includes the IO thread's scheduling latency and dates nothing. An
//! output route counts the same two latencies and no safety offset.
//!
//! Claude: a buffer whose time stamp has no valid host time goes undated,
//! never dated by when the IOProc ran. One with a valid sample time is also
//! counted by it, so a run of the source's frames holds every frame the device
//! captured over it, once.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use coreaudio_sys::*;
use spaghetti_common::ipc_protocol::{FlowState, FlowStatus};

use crate::abl;
use crate::audio;
use crate::delivery::DeliverySpan;
use crate::flow::{FlowMeter, Link};
use crate::host_time;
use crate::ring::{CHANNELS, Captured, Ring};
use crate::sinks::{MAX_SINKS, Sinks, SourceTiming};

/// Claude: how the first input stream's frames lie in the buffers an IOProc is
/// handed. A stream's buffers come first in the list, before other streams'.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// Claude: one buffer, `channels` samples to a frame.
    Interleaved { channels: usize },
    /// Claude: `channels` buffers of one sample to a frame.
    Planar { channels: usize },
}

impl Layout {
    /// Claude: how many frames `buffers` hold, or None when they do not have
    /// this layout's shape.
    fn frames(self, buffers: &[AudioBuffer]) -> Option<usize> {
        const SAMPLE: usize = size_of::<f32>();
        match self {
            Self::Interleaved { channels } => {
                let first = buffers.first()?;
                let bytes = first.mDataByteSize as usize;
                (first.mNumberChannels as usize == channels
                    && !first.mData.is_null()
                    && bytes.is_multiple_of(channels * SAMPLE))
                .then_some(bytes / (channels * SAMPLE))
            }
            Self::Planar { channels } => {
                let planes = buffers.get(..channels)?;
                let bytes = planes[0].mDataByteSize as usize;
                let read = &planes[..channels.min(CHANNELS)];
                (planes.iter().all(|plane| {
                    plane.mNumberChannels == 1 && plane.mDataByteSize as usize == bytes
                }) && read.iter().all(|plane| !plane.mData.is_null())
                    && bytes.is_multiple_of(SAMPLE))
                .then_some(bytes / SAMPLE)
            }
        }
    }

    /// Claude: `buffers` must have this layout's shape. Output channel `channel`
    /// of `frame`: the stream's channel of that number, or its only one.
    fn sample(self, buffers: &[AudioBuffer], frame: usize, channel: usize) -> f32 {
        let read = |buffer: &AudioBuffer, index: usize| {
            unsafe { abl::samples(buffer) }.map_or(0.0, |samples| samples[index])
        };
        match self {
            Self::Interleaved { channels } => {
                read(&buffers[0], frame * channels + channel.min(channels - 1))
            }
            Self::Planar { channels } => read(&buffers[channel.min(channels - 1)], frame),
        }
    }
}

/// Claude: when a buffer stamped `stamp` was captured, its input latency of
/// `latency_ticks` counted back, and where the device counts its first frame.
fn dated(stamp: Option<&AudioTimeStamp>, latency_ticks: u64) -> Captured {
    let Some(stamp) = stamp
        .filter(|stamp| stamp.mFlags & kAudioTimeStampHostTimeValid != 0 && stamp.mHostTime != 0)
    else {
        return Captured::Unknown;
    };
    let Some(at) = stamp.mHostTime.checked_sub(latency_ticks) else {
        return Captured::Unknown;
    };
    let counted = stamp.mFlags & kAudioTimeStampSampleTimeValid != 0
        && stamp.mSampleTime.is_finite()
        && stamp.mSampleTime >= 0.0;
    if counted {
        Captured::Counted {
            at,
            position: stamp.mSampleTime.round() as u64,
        }
    } else {
        Captured::Measured(at)
    }
}

/// Claude: what capture reads from a device and what places its frames in
/// time. Capture ends when any of it changes.
#[derive(Debug, Clone, PartialEq)]
struct InputFormat {
    stream: AudioObjectID,
    sample_rate: f64,
    layout: Layout,
    /// Claude: the device's input latency plus its first input stream's.
    latency_frames: u32,
    /// Claude: the channels of each input buffer, every input stream's.
    buffers: Vec<u32>,
}

impl InputFormat {
    fn read(device: AudioObjectID) -> Result<Self, String> {
        let stream = audio::first_stream(device, kAudioObjectPropertyScopeInput)
            .ok_or("the device has no input")?;
        let format =
            audio::virtual_format(stream).ok_or("could not read the input stream's format")?;
        let channels = format.mChannelsPerFrame as usize;
        if format.mFormatID != kAudioFormatLinearPCM
            || format.mFormatFlags & kAudioFormatFlagIsFloat == 0
            || format.mBitsPerChannel != 32
            || channels == 0
        {
            return Err(format!(
                "the input stream delivers format {:#x}, flags {:#x}, {} bits, {} channels at {} Hz; \
                 input capture takes 32-bit float",
                format.mFormatID,
                format.mFormatFlags,
                format.mBitsPerChannel,
                channels,
                format.mSampleRate
            ));
        }
        let nominal = audio::nominal_rate(device).ok_or("could not read the device's rate")?;
        if nominal != format.mSampleRate {
            return Err(format!(
                "the input stream runs at {} Hz and its device at {nominal} Hz",
                format.mSampleRate
            ));
        }
        let device_latency = audio::get_u32(
            device,
            kAudioDevicePropertyLatency,
            kAudioObjectPropertyScopeInput,
        )
        .ok_or("could not read the device's input latency")?;
        let stream_latency = audio::get_u32(
            stream,
            kAudioStreamPropertyLatency,
            kAudioObjectPropertyScopeGlobal,
        )
        .ok_or("could not read the input stream's latency")?;
        let buffers = audio::stream_configuration(device, kAudioObjectPropertyScopeInput)
            .ok_or("could not read the device's input buffers")?;
        let layout = if format.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0 {
            Layout::Planar { channels }
        } else {
            Layout::Interleaved { channels }
        };
        Ok(Self {
            stream,
            sample_rate: format.mSampleRate,
            layout,
            latency_frames: device_latency + stream_latency,
            buffers,
        })
    }

    fn latency_ticks(&self) -> u64 {
        (f64::from(self.latency_frames) / self.sample_rate * host_time::ticks_per_second()).round()
            as u64
    }

    /// Claude: what changed from `self` to `now`.
    fn change(&self, now: &Self) -> String {
        if now.sample_rate != self.sample_rate {
            format!(
                "its rate changed from {} Hz to {} Hz",
                self.sample_rate, now.sample_rate
            )
        } else if now.latency_frames != self.latency_frames {
            format!(
                "its input latency changed from {} to {} frames",
                self.latency_frames, now.latency_frames
            )
        } else if now.layout != self.layout {
            format!(
                "its input format changed from {:?} to {:?}",
                self.layout, now.layout
            )
        } else {
            "its input streams changed".into()
        }
    }
}

/// Claude: what a source shares with the capture it runs.
struct Shared {
    sinks: Sinks,
    delivery: Arc<DeliverySpan>,
    meter: FlowMeter,
    /// Claude: why capture ended, set once by a change to the device. Nothing
    /// is pushed after it, and the source does not capture again.
    ended: OnceLock<String>,
}

/// Claude: what the IOProc and the property listener read, for as long as
/// the capture runs.
struct Running {
    shared: Arc<Shared>,
    device: AudioObjectID,
    format: InputFormat,
    logged_once: AtomicBool,
}

impl Running {
    /// Claude: called on the HAL's notification thread. Ends capture when the
    /// device is gone or no longer reads as it did when capture started.
    fn check(&self) {
        if self.shared.ended.get().is_some() {
            return;
        }
        let alive = audio::get_u32(
            self.device,
            kAudioDevicePropertyDeviceIsAlive,
            kAudioObjectPropertyScopeGlobal,
        ) == Some(1);
        let reason = if alive {
            match InputFormat::read(self.device) {
                Err(reason) => format!("the input device changed: {reason}"),
                Ok(now) if now != self.format => {
                    format!("the input device changed: {}", self.format.change(&now))
                }
                Ok(_) => return,
            }
        } else {
            "the input device is gone".to_string()
        };
        tracing::warn!(%reason, "input capture ended");
        let _ = self.shared.ended.set(reason);
    }
}

/// Claude: the properties whose change ends capture.
fn watched(device: AudioObjectID, stream: AudioObjectID) -> [(AudioObjectID, u32, u32); 7] {
    [
        (
            device,
            kAudioDevicePropertyNominalSampleRate,
            kAudioObjectPropertyScopeGlobal,
        ),
        (
            device,
            kAudioDevicePropertyLatency,
            kAudioObjectPropertyScopeInput,
        ),
        (
            device,
            kAudioDevicePropertyStreamConfiguration,
            kAudioObjectPropertyScopeInput,
        ),
        (
            device,
            kAudioDevicePropertyStreams,
            kAudioObjectPropertyScopeInput,
        ),
        (
            device,
            kAudioDevicePropertyDeviceIsAlive,
            kAudioObjectPropertyScopeGlobal,
        ),
        (
            stream,
            kAudioStreamPropertyVirtualFormat,
            kAudioObjectPropertyScopeGlobal,
        ),
        (
            stream,
            kAudioStreamPropertyLatency,
            kAudioObjectPropertyScopeGlobal,
        ),
    ]
}

unsafe extern "C" fn input_changed(
    _object: AudioObjectID,
    _addresses: u32,
    _changed: *const AudioObjectPropertyAddress,
    client: *mut std::ffi::c_void,
) -> OSStatus {
    if let Some(running) = unsafe { client.cast::<Running>().as_ref() } {
        running.check();
    }
    0
}

/// Claude: a running IOProc on the device, and the listeners that end it on a
/// change. Whatever it acquired it releases when dropped, however far its
/// start got.
struct Capture {
    running: Arc<Running>,
    ioproc: AudioDeviceIOProcID,
    listening: Vec<(AudioObjectID, AudioObjectPropertyAddress)>,
    started: bool,
}

impl Capture {
    /// Claude: starts capture as `format` describes it, refused when the
    /// device no longer reads that way.
    fn start(
        device: AudioObjectID,
        format: InputFormat,
        shared: Arc<Shared>,
    ) -> Result<Self, String> {
        let stream = format.stream;
        let mut capture = Self {
            running: Arc::new(Running {
                shared,
                device,
                format,
                logged_once: AtomicBool::new(false),
            }),
            ioproc: None,
            listening: Vec::new(),
            started: false,
        };
        let client = Arc::as_ptr(&capture.running).cast_mut().cast();
        let status = unsafe {
            AudioDeviceCreateIOProcID(device, Some(input_ioproc), client, &mut capture.ioproc)
        };
        if status != 0 || capture.ioproc.is_none() {
            capture.ioproc = None;
            return Err(format!("AudioDeviceCreateIOProcID failed: {status:#010x}"));
        }
        for (object, selector, scope) in watched(device, stream) {
            let address = AudioObjectPropertyAddress {
                mSelector: selector,
                mScope: scope,
                mElement: kAudioObjectPropertyElementMain,
            };
            let status = unsafe {
                AudioObjectAddPropertyListener(object, &address, Some(input_changed), client)
            };
            if status != 0 {
                return Err(format!(
                    "could not watch the input device's property {selector:#x}: {status:#010x}"
                ));
            }
            capture.listening.push((object, address));
        }
        let now = InputFormat::read(device)?;
        if now != capture.running.format {
            return Err(format!(
                "the input device changed: {}",
                capture.running.format.change(&now)
            ));
        }
        let status = unsafe { AudioDeviceStart(device, capture.ioproc) };
        if status != 0 {
            return Err(format!("AudioDeviceStart (input) failed: {status:#010x}"));
        }
        capture.started = true;
        Ok(capture)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let device = self.running.device;
        let client = Arc::as_ptr(&self.running).cast_mut().cast();
        unsafe {
            if self.started {
                AudioDeviceStop(device, self.ioproc);
            }
            for (object, address) in &self.listening {
                AudioObjectRemovePropertyListener(*object, address, Some(input_changed), client);
            }
            if self.ioproc.is_some() {
                AudioDeviceDestroyIOProcID(device, self.ioproc);
            }
        }
    }
}

unsafe extern "C" fn input_ioproc(
    _device: AudioObjectID,
    _now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    input_time: *const AudioTimeStamp,
    _output: *mut AudioBufferList,
    _output_time: *const AudioTimeStamp,
    client: *mut std::ffi::c_void,
) -> OSStatus {
    let Some(running) = (unsafe { client.cast::<Running>().as_ref() }) else {
        return 0;
    };
    let shared = &running.shared;
    if input.is_null() || shared.ended.get().is_some() {
        return 0;
    }
    let now = host_time::now();
    let buffers = unsafe { abl::buffers(input) };
    let layout = running.format.layout;
    let Some(frames) = layout.frames(buffers) else {
        // Claude: a buffer of another shape than the stream's format: what it
        // held is lost to the routes.
        unsafe { shared.sinks.for_each(|ring| ring.discontinuity()) };
        return 0;
    };
    if frames == 0 {
        return 0;
    }
    let captured = dated(
        unsafe { input_time.as_ref() },
        running.format.latency_ticks(),
    );
    shared.delivery.publish(frames as f64);
    unsafe {
        shared.sinks.for_each(|ring| {
            ring.push_with(frames, now, captured, |frame, channel| {
                layout.sample(buffers, frame, channel)
            });
        });
    }
    shared
        .meter
        .record((frames * CHANNELS * size_of::<f32>()) as u64, now);
    if !running.logged_once.swap(true, Ordering::Relaxed) {
        tracing::info!(
            buffers = buffers.len(),
            frames,
            ?layout,
            ?captured,
            "input IOProc first call"
        );
    }
    0
}

/// Claude: what an input device captures. The device's IOProc runs, and its
/// listeners watch it, only while a consumer takes the frames. A change to
/// the device while capturing ends the source for good.
pub struct InputSource {
    device_uid: String,
    shared: Arc<Shared>,
    /// Claude: the format the source reported its timing by, which capture
    /// starts with and runs by until the last consumer leaves.
    format: Option<InputFormat>,
    capture: Option<Capture>,
    /// Claude: why capture last failed to start.
    refused: Option<String>,
    pub label: String,
}

impl InputSource {
    pub fn create(device_uid: &str) -> Result<Self, String> {
        let device = device(device_uid)?;
        InputFormat::read(device)
            .map_err(|reason| format!("input device '{device_uid}': {reason}"))?;
        Ok(Self {
            device_uid: device_uid.to_string(),
            shared: Arc::new(Shared {
                sinks: Sinks::new(),
                delivery: Arc::new(DeliverySpan::new(0.0)),
                meter: FlowMeter::new(),
                ended: OnceLock::new(),
            }),
            format: None,
            capture: None,
            refused: None,
            label: audio::device_name(device).unwrap_or_else(|| device_uid.to_string()),
        })
    }

    fn format(&mut self) -> Result<&InputFormat, String> {
        if let Some(reason) = self.shared.ended.get() {
            return Err(reason.clone());
        }
        if self.format.is_none() {
            self.format = Some(InputFormat::read(device(&self.device_uid)?)?);
        }
        Ok(self.format.as_ref().expect("read above"))
    }

    pub fn timing(&mut self) -> Result<SourceTiming, String> {
        let sample_rate = self.format()?.sample_rate;
        Ok(SourceTiming {
            sample_rate,
            delivery: Arc::clone(&self.shared.delivery),
        })
    }

    pub fn add_sink(&mut self, ring: Arc<Ring>) -> Result<usize, String> {
        let format = self.format()?.clone();
        let slot = self
            .shared
            .sinks
            .insert(Box::new(ring))
            .map_err(|_| format!("a source can feed at most {MAX_SINKS} routes"))?;
        if self.capture.is_none() {
            let started = device(&self.device_uid)
                .and_then(|device| Capture::start(device, format, Arc::clone(&self.shared)));
            match started {
                Ok(capture) => {
                    tracing::info!(label = self.label, "started input capture");
                    self.capture = Some(capture);
                    self.refused = None;
                }
                Err(reason) => {
                    drop(self.shared.sinks.remove(slot));
                    self.format = None;
                    self.refused = Some(reason.clone());
                    return Err(reason);
                }
            }
        }
        Ok(slot)
    }

    pub fn remove_sink(&mut self, slot: usize) {
        drop(self.shared.sinks.remove(slot));
        if self.shared.sinks.is_empty() {
            if self.capture.take().is_some() {
                tracing::info!(label = self.label, "stopped input capture");
            }
            self.format = None;
        }
    }

    /// Claude: `Lost` once a change ended capture; otherwise `Idle` without
    /// consumers and, with them, `Connected` until the device delivers.
    pub fn status(&self) -> FlowStatus {
        if self.shared.ended.get().is_some() {
            let mut status = self.shared.meter.status(Link::Idle);
            status.state = FlowState::Lost;
            return status;
        }
        let link = if self.capture.is_some() {
            Link::Connected {
                peer: None,
                packets_lost: 0,
                packets_retransmitted: 0,
            }
        } else {
            Link::Idle
        };
        self.shared.meter.status(link)
    }

    pub fn failure(&self) -> Option<String> {
        self.shared
            .ended
            .get()
            .cloned()
            .or_else(|| self.refused.clone())
    }
}

fn device(uid: &str) -> Result<AudioObjectID, String> {
    audio::find_device_by_uid(uid).ok_or_else(|| format!("input device '{uid}' not found"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::HostTime;
    use crate::program_io::Frames;
    use crate::router::Router;
    use crate::spec::{Destination, SourceSpec};

    fn stamp(flags: u32, host: u64, sample: f64) -> AudioTimeStamp {
        AudioTimeStamp {
            mSampleTime: sample,
            mHostTime: host,
            mRateScalar: 1.0,
            mWordClockTime: 0,
            mSMPTETime: unsafe { std::mem::zeroed() },
            mFlags: flags,
            mReserved: 0,
        }
    }

    const BOTH: u32 = kAudioTimeStampHostTimeValid | kAudioTimeStampSampleTimeValid;

    #[test]
    fn a_device_that_is_not_there_is_refused() {
        let mut router = Router::new();
        let refused = router
            .create_source(
                SourceSpec::Input {
                    device_uid: "spaghetti-no-such-device".into(),
                },
                None,
            )
            .unwrap_err();
        assert!(refused.contains("not found"), "{refused}");
    }

    #[test]
    fn a_buffer_is_dated_its_input_latency_before_its_time_stamp() {
        let stamped = stamp(BOTH, 1_000_000, 4096.0);
        assert_eq!(
            dated(Some(&stamped), 960),
            Captured::Counted {
                at: 1_000_000 - 960,
                position: 4096
            }
        );
        let format = InputFormat {
            stream: 0,
            sample_rate: 48_000.0,
            layout: Layout::Interleaved { channels: 2 },
            latency_frames: 480,
            buffers: vec![2],
        };
        let expected = 0.01 * host_time::ticks_per_second();
        assert!((format.latency_ticks() as f64 - expected).abs() <= 0.5);
    }

    #[test]
    fn a_buffer_without_a_valid_host_time_goes_undated() {
        for stamped in [
            stamp(kAudioTimeStampSampleTimeValid, 1_000_000, 4096.0),
            stamp(BOTH, 0, 4096.0),
            stamp(BOTH, 500, 4096.0),
        ] {
            assert_eq!(dated(Some(&stamped), 960), Captured::Unknown);
        }
        assert_eq!(dated(None, 0), Captured::Unknown);
    }

    #[test]
    fn a_buffer_without_a_valid_sample_time_is_dated_but_not_counted() {
        for stamped in [
            stamp(kAudioTimeStampHostTimeValid, 1_000_000, 4096.0),
            stamp(BOTH, 1_000_000, f64::NAN),
            stamp(BOTH, 1_000_000, -1.0),
        ] {
            assert_eq!(dated(Some(&stamped), 0), Captured::Measured(1_000_000));
        }
    }

    fn buffer(channels: u32, samples: &mut [f32]) -> AudioBuffer {
        AudioBuffer {
            mNumberChannels: channels,
            mDataByteSize: size_of_val(samples) as u32,
            mData: samples.as_mut_ptr().cast(),
        }
    }

    fn read(layout: Layout, buffers: &[AudioBuffer]) -> Option<Vec<[f32; CHANNELS]>> {
        let frames = layout.frames(buffers)?;
        Some(
            (0..frames)
                .map(|frame| std::array::from_fn(|ch| layout.sample(buffers, frame, ch)))
                .collect(),
        )
    }

    #[test]
    fn the_first_two_channels_are_read_from_an_interleaved_stream() {
        let mut four: Vec<f32> = (0..12).map(|s| s as f32).collect();
        let buffers = [buffer(4, &mut four)];
        let layout = Layout::Interleaved { channels: 4 };
        assert_eq!(
            read(layout, &buffers),
            Some(vec![[0.0, 1.0], [4.0, 5.0], [8.0, 9.0]])
        );
        assert_eq!(read(Layout::Interleaved { channels: 2 }, &buffers), None);
    }

    #[test]
    fn the_first_two_planes_are_read_from_a_planar_stream() {
        let mut planes: Vec<Vec<f32>> = (0..3)
            .map(|plane| (0..2).map(|f| (plane * 10 + f) as f32).collect())
            .collect();
        let buffers: Vec<AudioBuffer> = planes.iter_mut().map(|p| buffer(1, p)).collect();
        let layout = Layout::Planar { channels: 3 };
        assert_eq!(read(layout, &buffers), Some(vec![[0.0, 10.0], [1.0, 11.0]]));
        assert_eq!(read(Layout::Planar { channels: 4 }, &buffers), None);
    }

    #[test]
    fn one_channel_plays_on_both() {
        let mut mono = vec![0.25, -0.5];
        for layout in [
            Layout::Interleaved { channels: 1 },
            Layout::Planar { channels: 1 },
        ] {
            let buffers = [buffer(1, &mut mono)];
            assert_eq!(
                read(layout, &buffers),
                Some(vec![[0.25, 0.25], [-0.5, -0.5]])
            );
        }
    }

    #[test]
    fn a_multichannel_buffer_is_never_read_as_planes() {
        let mut stereo = vec![0.0; 8];
        let mut other = vec![0.0; 8];
        let buffers = [buffer(2, &mut stereo), buffer(2, &mut other)];
        assert_eq!(read(Layout::Planar { channels: 2 }, &buffers), None);
        let mut short = vec![0.0; 3];
        let mut long = vec![0.0; 4];
        let unequal = [buffer(1, &mut short), buffer(1, &mut long)];
        assert_eq!(read(Layout::Planar { channels: 2 }, &unequal), None);
    }

    /// Claude: where `burst` sounds in `samples`, by the frame it starts
    /// at within `search` frames of `around`, and how alike, from 0 to 1.
    fn find(samples: &[f32], burst: &[f32], around: f64, search: usize) -> (f64, f64) {
        let first = (around as usize).saturating_sub(search);
        let last = (around as usize + search).min(samples.len() - burst.len());
        let energy: f64 = burst.iter().map(|&s| f64::from(s).powi(2)).sum();
        let correlation = |lag: usize| -> f64 {
            let window = &samples[lag..lag + burst.len()];
            let dot: f64 = window
                .iter()
                .zip(burst)
                .map(|(&a, &b)| f64::from(a) * f64::from(b))
                .sum();
            let power: f64 = window.iter().map(|&s| f64::from(s).powi(2)).sum();
            dot / (energy * power).sqrt().max(f64::MIN_POSITIVE)
        };
        let scores: Vec<f64> = (first..=last).map(correlation).collect();
        let peak = (0..scores.len())
            .max_by(|&a, &b| scores[a].total_cmp(&scores[b]))
            .unwrap();
        let offset = if peak > 0 && peak + 1 < scores.len() {
            let (before, at, after) = (scores[peak - 1], scores[peak], scores[peak + 1]);
            0.5 * (before - after) / (before - 2.0 * at + after)
        } else {
            0.0
        };
        ((first + peak) as f64 + offset, scores[peak])
    }

    /// Claude: plays fresh noise bursts from a dated program source into a
    /// virtual device on a delay, and captures the device's loopback input,
    /// which reads back each frame at the time the device plays it. The
    /// device reports no latency either way, so each burst is captured dated
    /// when its route scheduled it to sound.
    #[test]
    #[ignore = "plays into the virtual device SPAGHETTI_LOOPBACK_UID names; run by scripts/input-loopback.sh"]
    fn a_loopback_input_dates_each_burst_when_its_route_played_it() {
        const RATE: f64 = 48_000.0;
        const DELAY: f64 = 0.2;
        const BLOCK: usize = 480;
        const BURST: usize = 1440;
        const BURSTS: usize = 8;
        let uid = std::env::var("SPAGHETTI_LOOPBACK_UID").expect("SPAGHETTI_LOOPBACK_UID");
        let tps = host_time::ticks_per_second();

        let mut router = Router::new();
        let mut probe = router.add_program_input("probe", RATE).unwrap();
        let played = router
            .create_source(
                SourceSpec::Program {
                    input: "probe".into(),
                },
                None,
            )
            .unwrap();
        router
            .create_route(
                played,
                Destination::Device {
                    uid: uid.clone(),
                    delay_ms: Some((DELAY * 1000.0) as u32),
                    latency_offset_ms: None,
                },
                1.0,
            )
            .unwrap();
        let input = router
            .create_source(SourceSpec::Input { device_uid: uid }, None)
            .unwrap();
        let listed = |router: &Router| {
            router
                .list_sources()
                .into_iter()
                .find(|source| source.source_id == input)
                .unwrap()
        };
        assert_eq!(listed(&router).status.state, FlowState::Idle);
        let (ring, timing) = router.attach_raw_sink(input);
        assert_eq!(
            timing.sample_rate, RATE,
            "the check's device runs at 48 kHz"
        );

        // Claude: the route plays the source at a fraction of a frame off the
        // device's frames, where white noise no longer matches itself, so the
        // noise is smoothed below a quarter of the rate first.
        let smoothing: Vec<f64> = (0..8)
            .map(|tap| {
                (std::f64::consts::PI * (tap as f64 + 0.5) / 8.0)
                    .sin()
                    .powi(2)
            })
            .collect();
        let mut noise = vec![0u8; BURSTS * (BURST + smoothing.len()) * 4];
        getrandom::fill(&mut noise).unwrap();
        let bursts: Vec<Vec<f32>> = noise
            .chunks(4)
            .map(|bytes| {
                f64::from(u32::from_le_bytes(bytes.try_into().unwrap())) / f64::from(u32::MAX) - 0.5
            })
            .collect::<Vec<f64>>()
            .chunks(BURST + smoothing.len())
            .map(|white| {
                (0..BURST)
                    .map(|n| {
                        let smoothed: f64 =
                            smoothing.iter().zip(&white[n..]).map(|(h, w)| h * w).sum();
                        (smoothed * 0.3) as f32
                    })
                    .collect()
            })
            .collect();
        let starts: Vec<usize> = (0..BURSTS)
            .map(|k| ((1.0 + 0.6 * k as f64) * RATE) as usize)
            .collect();
        let total = starts[BURSTS - 1] + RATE as usize;
        let origin = host_time::now();
        let date = move |frame: usize| origin + (frame as f64 / RATE * tps).round() as u64;

        let producer = std::thread::spawn({
            let (bursts, starts) = (bursts.clone(), starts.clone());
            move || {
                let sample = |frame: usize| {
                    starts
                        .iter()
                        .zip(&bursts)
                        .find(|(start, _)| (**start..**start + BURST).contains(&frame))
                        .map_or(0.0, |(start, burst)| burst[frame - start])
                };
                let mut next = 0;
                while next < total {
                    let due = ((host_time::now() - origin) as f64 / tps * RATE) as usize;
                    while next + BLOCK <= due && next < total {
                        let block: Vec<f32> = (next..next + BLOCK).map(sample).collect();
                        probe.push(
                            Frames::mono(&block).unwrap(),
                            HostTime::from_ticks(date(next)),
                        );
                        next += BLOCK;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                probe
            }
        });

        let ticks_per_frame = tps / RATE;
        let (mut samples, mut dates, mut runs, mut undated) = (Vec::new(), Vec::new(), 0, 0);
        let mut popped = vec![0.0; 256 * CHANNELS];
        let end = date(total) + ((DELAY + 0.5) * tps) as u64;
        while host_time::now() < end {
            if ring.take_discontinuity().is_some() {
                runs += 1;
            }
            while ring.available() > 0 {
                match ring.tail_captured(ticks_per_frame) {
                    Some(at) => dates.push((samples.len(), at as f64)),
                    None => undated += 1,
                }
                let frames = ring.pop_into(&mut popped);
                samples.extend(popped[..frames * CHANNELS].iter().step_by(CHANNELS));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _probe = producer.join().unwrap();
        assert!(matches!(
            listed(&router).status.state,
            FlowState::Connected | FlowState::Flowing
        ));
        assert_eq!(listed(&router).failure, None);
        assert_eq!((runs, undated), (1, 0), "runs and undated reads");
        assert_eq!((ring.discontinuities(), ring.overflows()), (0, 0));

        let index_at = |at: f64| {
            let (index, dated) = dates
                .iter()
                .rev()
                .find(|(_, dated)| *dated <= at)
                .expect("captured before the burst");
            *index as f64 + (at - dated) / ticks_per_frame
        };
        let date_at = |index: f64| {
            let (first, dated) = dates
                .iter()
                .rev()
                .find(|(first, _)| *first as f64 <= index)
                .unwrap();
            dated + (index - *first as f64) * ticks_per_frame
        };
        let mut residuals = Vec::new();
        for (k, (start, burst)) in starts.iter().zip(&bursts).enumerate() {
            let scheduled = date(*start) as f64 + DELAY * tps;
            let (found, alike) = find(&samples, burst, index_at(scheduled), (0.1 * RATE) as usize);
            let residual_ms = (date_at(found) - scheduled) / tps * 1000.0;
            println!(
                "burst {k}: captured {residual_ms:+.4} ms from its schedule, correlation {alike:.4}"
            );
            assert!(alike > 0.9, "burst {k} not found: correlation {alike}");
            residuals.push(residual_ms);
        }
        let mean = residuals.iter().sum::<f64>() / residuals.len() as f64;
        let spread = residuals.iter().copied().fold(f64::MIN, f64::max)
            - residuals.iter().copied().fold(f64::MAX, f64::min);
        println!("mean {mean:+.4} ms, spread {spread:.4} ms over {BURSTS} bursts");
        assert!(
            mean.abs() < 1.0 && spread < 0.1,
            "mean {mean} ms, spread {spread} ms"
        );

        router.destroy_source(input).unwrap();
    }

    /// Claude: capture runs while a route takes the device's frames, and the
    /// device's removal ends the source for good.
    #[test]
    #[ignore = "removes the virtual device SPAGHETTI_LOOPBACK_UID names; run by scripts/input-loopback.sh"]
    fn an_input_device_that_goes_away_ends_its_source_for_good() {
        let uid = std::env::var("SPAGHETTI_LOOPBACK_UID").expect("SPAGHETTI_LOOPBACK_UID");
        let mut router = Router::new();
        let _sink = router
            .add_program_output("sink", 48_000.0, Duration::ZERO)
            .unwrap();
        let to_sink = || Destination::Program {
            output: "sink".into(),
            delay_ms: None,
            latency_offset_ms: None,
        };
        let input = router
            .create_source(
                SourceSpec::Input {
                    device_uid: uid.clone(),
                },
                None,
            )
            .unwrap();
        let listed = |router: &Router| {
            router
                .list_sources()
                .into_iter()
                .find(|source| source.source_id == input)
                .unwrap()
        };
        assert_eq!(listed(&router).status.state, FlowState::Idle);
        let route = router.create_route(input, to_sink(), 1.0).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(listed(&router).status.state, FlowState::Flowing);
        router.destroy_route(route).unwrap();
        assert_eq!(listed(&router).status.state, FlowState::Idle);
        let route = router.create_route(input, to_sink(), 1.0).unwrap();

        let destroyed = std::process::Command::new("spaghetti")
            .args(["devices", "destroy", &uid])
            .status()
            .unwrap();
        assert!(destroyed.success());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while listed(&router).status.state != FlowState::Lost {
            assert!(
                std::time::Instant::now() < deadline,
                "{:?}",
                listed(&router)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let failure = listed(&router).failure.unwrap();
        println!("ended: {failure}");
        router.destroy_route(route).unwrap();
        assert_eq!(listed(&router).status.state, FlowState::Lost);
        let refused = router.create_route(input, to_sink(), 1.0).unwrap_err();
        assert_eq!(refused, failure);
    }
}
