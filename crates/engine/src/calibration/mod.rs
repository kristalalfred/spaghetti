//! Claude: acoustic latency calibration. Probe trains play through two
//! destinations, each on its own schedule, and one recording hears them: the
//! reference, the output, then the reference again. Each train arrives some
//! time late against its schedule; the output's lateness less the
//! reference's, interpolated to when the output played, is how much later
//! the output sounds than the reference where the recording is made, which
//! no bias of the recording's own dates changes. Adding it to the output's
//! latency offset makes both sound together there.
//!
//! Claude: `Router::calibrate` returns at once. A worker thread owns
//! everything the calibration plays and records, and releases all of it when
//! the calibration ends, fails or is cancelled, before it reports so.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::spec::Destination;

mod detect;
mod emitter;
mod job;
mod probe;
mod recorder;
#[cfg(test)]
mod tests;

pub(crate) use emitter::timing as emitter_timing;
pub(crate) use job::{Feed, Job, Leg};

/// Claude: what to calibrate: `output` against `reference`, both destinations
/// on a delay, as heard through `recording`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Calibration {
    pub output: Destination,
    pub reference: Destination,
    pub recording: Recording,
}

/// Claude: what hears the probes. It must date what it captures by when it
/// captured it, as input devices and program inputs do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recording {
    /// Claude: a Core Audio input device, captured by the calibration itself.
    Input { device_uid: String },
    /// Claude: the program input registered under this name, as the program
    /// pushes it.
    Program { input: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Output,
    Reference,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Output => "output",
            Self::Reference => "reference",
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CalibrationState {
    /// Claude: waiting for this destination to play before probing it.
    Waiting(Role),
    Probing(Role),
    Analyzing,
    Done(Result<Calibrated, CalibrationFailure>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Calibrated {
    pub output: Residual,
    /// Claude: the reference as when the output played, between its two
    /// trains.
    pub reference: Residual,
    /// Claude: the output's latency offset that makes it sound with the
    /// reference where the recording was made.
    pub latency_offset_ms: i32,
}

/// Claude: how much later than scheduled a destination was heard, and how
/// far apart its probes' arrivals lay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Residual {
    pub late_ms: f64,
    pub spread_ms: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CalibrationFailure {
    /// Claude: the destination could not play.
    Unavailable {
        role: Role,
        reason: String,
    },
    /// Claude: the destination did not play a train on its schedule throughout.
    Glitched {
        role: Role,
    },
    Probes {
        role: Role,
        failure: ProbeFailure,
    },
    Recording(RecordingFailure),
    /// Claude: the router, or a program input or output the calibration
    /// used, went away.
    EndpointRemoved,
    Cancelled,
}

impl std::fmt::Display for CalibrationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { role, reason } => write!(f, "the {role} could not play: {reason}"),
            Self::Glitched { role } => {
                write!(
                    f,
                    "the {role} did not play its probes on schedule throughout"
                )
            }
            Self::Probes { role, failure } => write!(f, "{role}: {failure}"),
            Self::Recording(failure) => failure.fmt(f),
            Self::EndpointRemoved => f.write_str("a program input or output it used was removed"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for CalibrationFailure {}

/// Claude: why a train was not measured.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeFailure {
    /// Claude: nothing in the recording matched the train as clearly as it
    /// must; the loudest it heard stood this far above the background.
    NotFound { above_background_db: f64 },
    /// Claude: something matched the train before the arrival, too faint to
    /// measure and too clear to ignore.
    Ambiguous,
}

impl std::fmt::Display for ProbeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound {
                above_background_db,
            } => write!(
                f,
                "the probes were not heard: the loudest sound stood {above_background_db:.0} dB \
                 above the background, short of 20 dB"
            ),
            Self::Ambiguous => f.write_str(
                "the probes were heard more than once, the earliest too faintly to measure",
            ),
        }
    }
}

/// Claude: why the recording cannot be measured in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingFailure {
    /// Claude: capture did not start, or ended, for this reason.
    Unavailable(String),
    /// Claude: nothing arrived for a while.
    Stalled,
    /// Claude: frames the source did not date.
    Undated,
    /// Claude: frames missing or repeated.
    Gap,
    Clipped,
    NonFinite,
}

impl std::fmt::Display for RecordingFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(reason) => write!(f, "the recording could not go on: {reason}"),
            Self::Stalled => f.write_str("the recording stopped delivering audio"),
            Self::Undated => f.write_str("the recording has frames its source did not date"),
            Self::Gap => f.write_str("the recording misses or repeats frames"),
            Self::Clipped => {
                f.write_str("the recording clipped: lower the volume or move the microphone away")
            }
            Self::NonFinite => f.write_str("the recording has samples that are not numbers"),
        }
    }
}

/// Claude: what the worker shares with the handle.
struct Progress {
    state: Mutex<CalibrationState>,
    cancelled: Mutex<bool>,
    wake: Condvar,
}

impl Progress {
    fn cancelled(&self) -> MutexGuard<'_, bool> {
        self.cancelled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn is_cancelled(&self) -> bool {
        *self.cancelled()
    }

    fn cancel(&self) {
        *self.cancelled() = true;
        self.wake.notify_all();
    }

    /// Claude: waits `timeout`, or until cancelled.
    fn pause(&self, timeout: Duration) {
        let cancelled = self.cancelled();
        let _ = self
            .wake
            .wait_timeout_while(cancelled, timeout, |cancelled| !*cancelled);
    }

    fn state(&self) -> MutexGuard<'_, CalibrationState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Claude: a calibration under way. Dropping it cancels the calibration
/// without waiting for it.
pub struct Calibrating {
    progress: Arc<Progress>,
}

impl Calibrating {
    pub(crate) fn start(job: Job) -> Result<Self, String> {
        let progress = Arc::new(Progress {
            state: Mutex::new(job.state()),
            cancelled: Mutex::new(false),
            wake: Condvar::new(),
        });
        let worker = Arc::clone(&progress);
        std::thread::Builder::new()
            .name("spaghetti-calibration".into())
            .spawn(move || {
                let mut job = job;
                let result = job.run(
                    &|| worker.is_cancelled(),
                    &|state| *worker.state() = state,
                    &|timeout| worker.pause(timeout),
                );
                job.release();
                *worker.state() = CalibrationState::Done(result);
            })
            .map_err(|e| format!("could not start the calibration's thread: {e}"))?;
        Ok(Self { progress })
    }

    pub fn state(&self) -> CalibrationState {
        self.progress.state().clone()
    }

    /// Claude: ends the calibration. Its state turns `Done(Err(Cancelled))`
    /// once it released what it played and recorded through.
    pub fn cancel(&self) {
        self.progress.cancel();
    }
}

impl Drop for Calibrating {
    fn drop(&mut self) {
        self.progress.cancel();
    }
}
