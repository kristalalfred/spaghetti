/// Standard sample rate for all virtual devices.
pub const SAMPLE_RATE: f64 = 48000.0;
/// Channels per stream.
pub const CHANNELS: u32 = 2;
/// Bytes per sample (f32).
pub const BYTES_PER_SAMPLE: u32 = 4;
/// Bytes per frame (channels * bytes_per_sample).
pub const BYTES_PER_FRAME: u32 = CHANNELS * BYTES_PER_SAMPLE;
/// Ring buffer size in frames (power of 2 for masking).
pub const RING_BUFFER_FRAMES: usize = 16384;
