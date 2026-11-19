//! Claude: a route's output to a render endpoint: a shared-mode stream of
//! its own, at the rate the audio engine mixes the endpoint at, fed from a
//! thread woken each time the stream has room. The stream's clock gives the
//! position of the frame playing now, so the frames written next play
//! `written - played` frames later, and the clock already counts the
//! endpoint's latency.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use spaghetti_common::ipc_protocol::FlowStatus;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient,
    IAudioClock, IAudioRenderClient,
};

use super::audio::DeviceId;
use super::com::{self, Event, ProAudio};
use crate::flow::{FlowMeter, Link};
use crate::host_time;
use crate::mixer::{Cycles, Fader};
use crate::playout::{Playout, PlayoutHealth, Schedule};
use crate::ring::{CHANNELS, Ring};
use crate::sinks::SourceTiming;
use crate::spec::Playback;

/// Claude: the stream's buffer, in 100 ns units.
const BUFFER_HNS: i64 = 200_000;
const POLL_MILLIS: u32 = 100;

pub struct DeviceOutput {
    control: Arc<Control>,
    thread: Option<JoinHandle<()>>,
    health: Arc<PlayoutHealth>,
    meter: Arc<FlowMeter>,
    cycles: Arc<Cycles>,
    reported_glitches: [u32; 6],
    scheduled: bool,
}

struct Control {
    stop: AtomicBool,
    /// Claude: set by `start`, answered by the thread with whether the stream started.
    start: Mutex<Option<mpsc::SyncSender<Result<(), String>>>>,
}

fn rate(device: &str) -> Result<u32, String> {
    let rate = com::mix_format(&com::activate(&com::device(device)?)?)?.rate;
    if rate == 0 {
        return Err("could not read the device's rate".into());
    }
    Ok(rate)
}

/// Claude: the clock places each frame where it sounds, so only the offset
/// counts as latency.
fn schedule(delay: f64, latency_offset_ms: Option<i32>) -> Schedule {
    Schedule {
        delay,
        latency: f64::from(latency_offset_ms.unwrap_or(0)) / 1000.0,
    }
}

/// Claude: refused as opening a route from `source` to the device would be
/// for its delay, without opening anything.
pub fn check_schedule(
    device: &DeviceId,
    source: &SourceTiming,
    delay: f64,
    latency_offset_ms: Option<i32>,
) -> Result<(), String> {
    let rate = f64::from(rate(device)?);
    let schedule = schedule(delay, latency_offset_ms);
    Playout::new(source, rate, host_time::ticks_per_second(), Some(schedule)).map(|_| ())
}

impl DeviceOutput {
    /// # Safety
    ///
    /// Claude: safe; it shares the signature of the Core Audio backend's.
    pub unsafe fn open(
        device: DeviceId,
        source: SourceTiming,
        volume: Arc<AtomicU32>,
        delay: Option<f64>,
        latency_offset_ms: Option<i32>,
    ) -> Result<(Self, Arc<Ring>), String> {
        let rate = rate(&device)?;
        let schedule = delay.map(|delay| schedule(delay, latency_offset_ms));
        if let Some(schedule) = schedule {
            tracing::info!(
                delay_ms = schedule.delay * 1000.0,
                latency_ms = schedule.latency * 1000.0,
                "scheduled playout"
            );
        }
        let playout = Playout::new(
            &source,
            f64::from(rate),
            host_time::ticks_per_second(),
            schedule,
        )?;
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
        let control = Arc::new(Control {
            stop: AtomicBool::new(false),
            start: Mutex::new(None),
        });
        let state = RenderState {
            ring: Arc::clone(&ring),
            fader: Fader::new(
                f32::from_bits(volume.load(Ordering::Relaxed)),
                f64::from(rate),
            ),
            playout,
            volume,
            meter: Arc::clone(&meter),
            cycles: Arc::clone(&cycles),
            planes: [Vec::new(), Vec::new()],
            logged_once: false,
        };
        let (opened, ready) = mpsc::sync_channel(1);
        let thread = {
            let control = Arc::clone(&control);
            thread::Builder::new()
                .name("spaghetti-render".into())
                .spawn(move || render_thread(&device, rate, state, &control, opened))
                .map_err(|e| format!("start the render thread: {e}"))?
        };
        let output = Self {
            control,
            thread: Some(thread),
            health,
            meter,
            cycles,
            reported_glitches: [0; 6],
            scheduled: schedule.is_some(),
        };
        ready
            .recv()
            .map_err(|_| "the render thread stopped before opening".to_string())??;
        Ok((output, ring))
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
        let (started, answer) = mpsc::sync_channel(1);
        *self
            .control
            .start
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(started);
        answer
            .recv()
            .map_err(|_| "the render thread stopped".to_string())?
    }

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

impl Drop for DeviceOutput {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct RenderState {
    ring: Arc<Ring>,
    playout: Playout,
    volume: Arc<AtomicU32>,
    fader: Fader,
    meter: Arc<FlowMeter>,
    cycles: Arc<Cycles>,
    planes: [Vec<f32>; CHANNELS],
    logged_once: bool,
}

struct Stream {
    client: IAudioClient,
    render: IAudioRenderClient,
    clock: IAudioClock,
    event: Event,
    size: u32,
    /// Claude: the clock's units per second.
    frequency: f64,
}

impl Stream {
    fn open(device: &str, rate: u32) -> Result<Self, String> {
        let client = com::activate(&com::device(device)?)?;
        let format = com::stereo_float(rate);
        unsafe {
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    BUFFER_HNS,
                    0,
                    &format,
                    None,
                )
                .map_err(|e| format!("open the output stream: {e}"))?;
        }
        let event = Event::new()?;
        let opened = |e: windows::core::Error| format!("open the output stream: {e}");
        unsafe {
            client.SetEventHandle(event.0).map_err(opened)?;
            let size = client.GetBufferSize().map_err(opened)?;
            let render: IAudioRenderClient = client.GetService().map_err(opened)?;
            let clock: IAudioClock = client.GetService().map_err(opened)?;
            let frequency = clock.GetFrequency().map_err(opened)? as f64;
            Ok(Self {
                client,
                render,
                clock,
                event,
                size,
                frequency,
            })
        }
    }

    /// Claude: the host time the next frame written plays at, from where
    /// the clock says playback is now.
    fn next_output(&self, written: u64, rate: f64, now: u64) -> Option<u64> {
        let mut position = 0u64;
        let mut qpc = 0u64;
        unsafe { self.clock.GetPosition(&mut position, Some(&mut qpc)) }.ok()?;
        if qpc == 0 {
            return None;
        }
        let played = position as f64 / self.frequency;
        let ahead = written as f64 / rate - played;
        let at = com::hns_to_ticks(qpc) as f64 + ahead * host_time::ticks_per_second();
        Some(if at > 0.0 { at as u64 } else { now })
    }
}

fn render_thread(
    device: &str,
    rate: u32,
    mut state: RenderState,
    control: &Control,
    opened: mpsc::SyncSender<Result<(), String>>,
) {
    com::init();
    let _scheduled = ProAudio::join();
    let stream = match Stream::open(device, rate) {
        Ok(stream) => stream,
        Err(reason) => {
            let _ = opened.send(Err(reason));
            return;
        }
    };
    let frames = stream.size as usize;
    state.planes = [vec![0.0; frames], vec![0.0; frames]];
    let _ = opened.send(Ok(()));

    let started = loop {
        if control.stop.load(Ordering::Relaxed) {
            return;
        }
        if let Some(started) = control
            .start
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            break started;
        }
        thread::park_timeout(std::time::Duration::from_millis(5));
    };
    let mut written = 0u64;
    let primed = unsafe {
        stream
            .render
            .GetBuffer(stream.size)
            .and_then(|_| {
                stream
                    .render
                    .ReleaseBuffer(stream.size, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
            })
            .and_then(|()| stream.client.Start())
    };
    if let Err(e) = primed {
        let _ = started.send(Err(format!("start the output stream: {e}")));
        return;
    }
    written += u64::from(stream.size);
    let _ = started.send(Ok(()));

    while !control.stop.load(Ordering::Relaxed) {
        stream.event.wait(POLL_MILLIS);
        if let Err(e) = render(&stream, &mut state, f64::from(rate), &mut written) {
            tracing::warn!(device, "the output stream failed: {e}");
            break;
        }
    }
    let _ = unsafe { stream.client.Stop() };
}

fn render(
    stream: &Stream,
    state: &mut RenderState,
    rate: f64,
    written: &mut u64,
) -> windows::core::Result<()> {
    let padding = unsafe { stream.client.GetCurrentPadding() }?;
    let free = stream.size.saturating_sub(padding);
    if free == 0 {
        return Ok(());
    }
    let frames = free as usize;
    let now = host_time::now();
    let output = stream.next_output(*written, rate, now).unwrap_or_else(|| {
        state.playout.count_unstamped_render();
        now
    });
    let [left, right] = &mut state.planes;
    let mut out = [&mut left[..frames], &mut right[..frames]];
    let ring_frames = state.ring.available();
    state.playout.render(&state.ring, &mut out, now, output);
    if state.playout.is_playing() {
        state
            .meter
            .record((frames * CHANNELS * size_of::<f32>()) as u64, now);
    }
    let volume = f32::from_bits(state.volume.load(Ordering::Relaxed));
    state.fader.planar(&mut out, volume);
    unsafe {
        let data = stream.render.GetBuffer(free)?;
        let samples = std::slice::from_raw_parts_mut(data.cast::<f32>(), frames * CHANNELS);
        for (frame, pair) in samples.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
            *pair = [out[0][frame], out[1][frame]];
        }
        stream.render.ReleaseBuffer(free, 0)?;
    }
    *written += u64::from(free);
    state.cycles.tick(now, frames, rate);
    if !state.logged_once {
        state.logged_once = true;
        tracing::info!(
            frames,
            ring_frames,
            peak_abs_f32 = out[0].iter().fold(0.0f32, |p, s| p.max(s.abs())),
            "render first fill"
        );
    }
    Ok(())
}
