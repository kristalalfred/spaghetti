//! Claude: acoustic latency calibration: probes played through two
//! destinations on their own schedules, recorded by one microphone, find how
//! much later or earlier than its schedule each one sounds.

mod emitter;
mod probe;
mod recorder;

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
