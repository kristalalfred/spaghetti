//! Claude: acoustic latency calibration: probes played through two
//! destinations on their own schedules, recorded by one microphone, find how
//! much later or earlier than its schedule each one sounds.

mod detect;
mod emitter;
mod probe;
mod recorder;

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
    /// Claude: frames the source did not date.
    Undated,
    /// Claude: frames missing or repeated.
    Gap,
    Clipped,
    NonFinite,
}

impl std::fmt::Display for RecordingFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Undated => "the recording has frames its source did not date",
            Self::Gap => "the recording misses or repeats frames",
            Self::Clipped => "the recording clipped: lower the volume or move the microphone away",
            Self::NonFinite => "the recording has samples that are not numbers",
        })
    }
}
