//! Claude: a shared-mode capture stream feeding a source's rings: an input
//! device's, or an app's through process loopback. A thread of its own runs
//! the stream, woken by it each time a buffer is ready.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient,
    IAudioClient,
};

use super::com::{self, Event, ProAudio};
use crate::delivery::DeliverySpan;
use crate::flow::FlowMeter;
use crate::host_time;
use crate::mixer::Cycles;
use crate::ring::{CHANNELS, Captured, Ring};
use crate::sinks::{MAX_SINKS, Sinks};

/// Claude: the stream's buffer, in 100 ns units.
const BUFFER_HNS: i64 = 200_000;
const POLL_MILLIS: u32 = 100;

/// Claude: what the capture thread feeds and the source reads.
pub struct Shared {
    sinks: Sinks,
    pub delivery: Arc<DeliverySpan>,
    pub meter: FlowMeter,
    pub cycles: Cycles,
    /// Claude: why capture ended for good, set once. Nothing is pushed after it.
    pub ended: OnceLock<String>,
    /// Claude: the rings in `sinks` by slot, which an end marks ended.
    consumers: Mutex<Vec<(usize, Arc<Ring>)>>,
}

impl Shared {
    pub fn new() -> Self {
        Self {
            sinks: Sinks::new(),
            delivery: Arc::new(DeliverySpan::new(0.0)),
            meter: FlowMeter::new(),
            cycles: Cycles::new(),
            ended: OnceLock::new(),
            consumers: Mutex::new(Vec::new()),
        }
    }

    fn consumers(&self) -> MutexGuard<'_, Vec<(usize, Arc<Ring>)>> {
        self.consumers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn attach(&self, ring: Arc<Ring>) -> Result<usize, String> {
        let mut consumers = self.consumers();
        if let Some(reason) = self.ended.get() {
            return Err(reason.clone());
        }
        let slot = self
            .sinks
            .insert(Box::new(Arc::clone(&ring)))
            .map_err(|_| format!("a source can feed at most {MAX_SINKS} routes"))?;
        consumers.push((slot, ring));
        Ok(slot)
    }

    pub fn detach(&self, slot: usize) {
        self.consumers().retain(|(attached, _)| *attached != slot);
        drop(self.sinks.remove(slot));
    }

    pub fn is_empty(&self) -> bool {
        self.sinks.is_empty()
    }

    fn end(&self, reason: String) {
        let consumers = self.consumers();
        if self.ended.set(reason.clone()).is_ok() {
            tracing::warn!(%reason, "capture ended");
            for (_, ring) in consumers.iter() {
                ring.end();
            }
        }
    }
}

/// Claude: how a stream's buffers are dated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dating {
    /// Claude: by the performance counter time of each buffer's first frame,
    /// placed by the device's frame position.
    Counted,
    /// Claude: by the performance counter time alone. Process loopback mixes
    /// an app's streams, so its positions count no device's frames.
    Measured,
}

/// Claude: how a stream opens: the client to initialize, and whether it is a
/// loopback stream.
pub struct Opening {
    pub client: IAudioClient,
    pub loopback: bool,
}

/// Claude: a running capture thread. Dropping it stops the stream.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Claude: opens the stream on a thread of its own with `open` and
    /// returns once it runs, or with why it could not start.
    pub fn start(
        label: &str,
        rate: u32,
        dating: Dating,
        shared: Arc<Shared>,
        open: impl FnOnce() -> Result<Opening, String> + Send + 'static,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, started) = mpsc::sync_channel(1);
        let thread = {
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name(format!("spaghetti-capture {label}"))
                .spawn(move || {
                    com::init();
                    let _scheduled = ProAudio::join();
                    let stream = match Stream::open(open, rate) {
                        Ok(stream) => {
                            let _ = ready.send(Ok(()));
                            stream
                        }
                        Err(reason) => {
                            let _ = ready.send(Err(reason));
                            return;
                        }
                    };
                    if let Err(reason) = stream.run(rate, dating, &shared, &stop) {
                        shared.end(reason);
                    }
                    let _ = unsafe { stream.client.Stop() };
                })
                .map_err(|e| format!("start the capture thread: {e}"))?
        };
        let capture = Self {
            stop,
            thread: Some(thread),
        };
        started
            .recv()
            .map_err(|_| "the capture thread stopped before starting".to_string())??;
        Ok(capture)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Stream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
}

impl Stream {
    fn open(open: impl FnOnce() -> Result<Opening, String>, rate: u32) -> Result<Self, String> {
        let Opening { client, loopback } = open()?;
        let format = com::stereo_float(rate);
        let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        unsafe {
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    BUFFER_HNS,
                    0,
                    &format,
                    None,
                )
                .map_err(|e| format!("open the capture stream: {e}"))?;
        }
        let event = Event::new()?;
        unsafe { client.SetEventHandle(event.0) }
            .map_err(|e| format!("set the capture event: {e}"))?;
        let capture: IAudioCaptureClient =
            unsafe { client.GetService() }.map_err(|e| format!("open the capture client: {e}"))?;
        unsafe { client.Start() }.map_err(|e| format!("start capture: {e}"))?;
        Ok(Self {
            client,
            capture,
            event,
        })
    }

    /// Claude: runs until `stop`, or until the stream fails, with why.
    fn run(
        &self,
        rate: u32,
        dating: Dating,
        shared: &Shared,
        stop: &AtomicBool,
    ) -> Result<(), String> {
        let rate = f64::from(rate);
        let mut logged = false;
        while !stop.load(Ordering::Relaxed) {
            self.event.wait(POLL_MILLIS);
            loop {
                let pending = unsafe { self.capture.GetNextPacketSize() }
                    .map_err(|e| format!("the capture stream failed: {e}"))?;
                if pending == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let mut position = 0u64;
                let mut qpc = 0u64;
                unsafe {
                    self.capture.GetBuffer(
                        &mut data,
                        &mut frames,
                        &mut flags,
                        Some(&mut position),
                        Some(&mut qpc),
                    )
                }
                .map_err(|e| format!("the capture stream failed: {e}"))?;
                let now = host_time::now();
                let frames_n = frames as usize;
                let dated = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 && qpc > 0;
                let at = com::hns_to_ticks(qpc);
                let captured = match dating {
                    Dating::Counted if dated => Captured::Counted { at, position },
                    Dating::Measured if dated => Captured::Measured(at),
                    _ => Captured::Unknown,
                };
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
                let samples: &[f32] = if silent {
                    &[]
                } else {
                    unsafe { std::slice::from_raw_parts(data.cast(), frames_n * CHANNELS) }
                };
                if frames_n > 0 {
                    shared.delivery.publish(frames_n as f64);
                }
                unsafe {
                    shared.sinks.for_each(|ring| {
                        ring.push_with(frames_n, now, captured, |f, ch| {
                            samples.get(f * CHANNELS + ch).copied().unwrap_or(0.0)
                        });
                    })
                };
                shared
                    .meter
                    .record((frames_n * CHANNELS * size_of::<f32>()) as u64, now);
                shared.cycles.tick(now, frames_n, rate);
                unsafe { self.capture.ReleaseBuffer(frames) }
                    .map_err(|e| format!("the capture stream failed: {e}"))?;
                if !logged {
                    logged = true;
                    tracing::info!(frames, flags, ?captured, "capture first buffer");
                }
            }
        }
        Ok(())
    }
}
