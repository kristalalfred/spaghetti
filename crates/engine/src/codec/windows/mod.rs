//! Claude: Opus through libopus, which encodes and decodes at 48 kHz, and
//! AAC through Media Foundation. A source at a rate the encoder does not take
//! is resampled first, without the resampler's delay, so packets keep the
//! timing of what was captured; AAC decoded at another rate is resampled to
//! `DECODED_RATE` with the delay kept, so each unit after the first decodes
//! to its whole length. libopus decodes every frame of a packet, the encoder's
//! look-ahead of `OPUS_DELAY_FRAMES` included, so decoded frames end where
//! the packet's span ends from the first packet on.

mod aac;

use opus::{Application, Bitrate, Channels};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, Resampler, SincInterpolationParameters, WindowFunction};

use super::{Codec, DECODED_RATE, StreamFormat};
use crate::ring::CHANNELS;

/// Claude: Media Foundation's AAC encoder takes 44.1 and 48 kHz. libopus
/// takes 8, 12, 16, 24 and 48 kHz; streams here use 48.
pub(super) fn encode_rates(codec: Codec) -> &'static [u32] {
    match codec {
        Codec::Aac => &[48000, 44100],
        Codec::Opus => &[48000],
    }
}

/// Claude: the largest packet a 20 ms frame encodes to, per RFC 6716.
const MAX_PACKET: usize = 1275;
/// Claude: 120 ms, the longest an Opus packet decodes to.
const MAX_DECODED_FRAMES: usize = 5760;
const SINC_LEN: usize = 128;

/// Claude: resamples interleaved frames from one rate to another, a chunk at
/// a time. An aligned converter drops the frames the resampler delays its
/// output by, so its output lines up with its input.
struct Converter {
    resampler: Async<f32>,
    /// Claude: output frames still to drop.
    delay: usize,
    channels: usize,
    pending: Vec<f32>,
    output: Vec<f32>,
}

impl Converter {
    /// Claude: converts `chunk` frames at a time, so input that arrives in
    /// chunks comes out in step with it.
    fn new(
        from: f64,
        to: f64,
        channels: usize,
        chunk: usize,
        aligned: bool,
    ) -> Result<Self, String> {
        let params = SincInterpolationParameters::new(SINC_LEN, WindowFunction::BlackmanHarris2);
        let resampler =
            Async::<f32>::new_sinc(to / from, 1.0, &params, chunk, channels, FixedAsync::Input)
                .map_err(|e| format!("create a resampler: {e}"))?;
        let output = vec![0.0; resampler.output_frames_max() * channels];
        Ok(Self {
            delay: if aligned { resampler.output_delay() } else { 0 },
            resampler,
            channels,
            pending: Vec::new(),
            output,
        })
    }

    fn push(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<(), String> {
        self.pending.extend_from_slice(input);
        let channels = self.channels;
        loop {
            let need = self.resampler.input_frames_next();
            if self.pending.len() < need * channels {
                return Ok(());
            }
            let max = self.resampler.output_frames_max();
            let input = InterleavedSlice::new(&self.pending[..need * channels], channels, need)
                .map_err(|e| format!("resample: {e}"))?;
            let mut output =
                InterleavedSlice::new_mut(&mut self.output[..max * channels], channels, max)
                    .map_err(|e| format!("resample: {e}"))?;
            let (read, written) = self
                .resampler
                .process_into_buffer(&input, &mut output, None)
                .map_err(|e| format!("resample: {e}"))?;
            let dropped = self.delay.min(written);
            self.delay -= dropped;
            out.extend_from_slice(&self.output[dropped * channels..written * channels]);
            self.pending.drain(..read * channels);
        }
    }
}

enum Encoding {
    Opus(opus::Encoder),
    Aac(aac::AacEncoder),
}

/// Claude: encodes interleaved stereo f32 at a source's rate into packets of
/// `frames_per_packet` frames at `format().sample_rate`.
pub struct Encoder {
    codec: Codec,
    format: StreamFormat,
    encoding: Encoding,
    converter: Option<Converter>,
    pending: Vec<f32>,
    packet: Vec<u8>,
}

impl Encoder {
    pub fn new(codec: Codec, source_rate: u32) -> Result<Self, String> {
        let format = StreamFormat {
            sample_rate: codec.encode_rate(source_rate),
            channels: CHANNELS as u8,
        };
        let encoding = match codec {
            Codec::Opus => {
                let mut opus =
                    opus::Encoder::new(format.sample_rate, Channels::Stereo, Application::Audio)
                        .map_err(|e| format!("create the {codec} encoder: {e}"))?;
                opus.set_bitrate(Bitrate::Bits(codec.bitrate() as i32))
                    .map_err(|e| format!("set the {codec} bitrate: {e}"))?;
                Encoding::Opus(opus)
            }
            Codec::Aac => Encoding::Aac(aac::AacEncoder::new(format, codec.bitrate())?),
        };
        let converter = (format.sample_rate != source_rate)
            .then(|| {
                Converter::new(
                    f64::from(source_rate),
                    f64::from(format.sample_rate),
                    CHANNELS,
                    codec.frames_per_packet() as usize,
                    true,
                )
            })
            .transpose()?;
        Ok(Self {
            codec,
            format,
            encoding,
            converter,
            pending: Vec::new(),
            packet: vec![0; MAX_PACKET],
        })
    }

    pub fn format(&self) -> StreamFormat {
        self.format
    }

    pub fn frames_per_packet(&self) -> usize {
        self.codec.frames_per_packet() as usize
    }

    /// Claude: the frames an AAC encoder's output trails its input by, as
    /// Media Foundation's own decoder plays it: none, measured round trip.
    /// Opus streams count their look-ahead in `OPUS_DELAY_FRAMES` instead.
    pub fn priming_frames(&self) -> usize {
        0
    }

    /// Claude: feeds interleaved stereo frames at the source rate and hands each
    /// packet they complete to `each`, in order.
    pub fn encode(&mut self, pcm: &[f32], mut each: impl FnMut(&[u8])) -> Result<(), String> {
        match &mut self.converter {
            Some(converter) => converter.push(pcm, &mut self.pending)?,
            None => self.pending.extend_from_slice(pcm),
        }
        let samples = self.frames_per_packet() * CHANNELS;
        let mut start = 0;
        while self.pending.len() - start >= samples {
            let chunk = &self.pending[start..start + samples];
            match &mut self.encoding {
                Encoding::Opus(opus) => {
                    let len = opus
                        .encode_float(chunk, &mut self.packet)
                        .map_err(|e| format!("{} encode failed: {e}", self.codec))?;
                    each(&self.packet[..len]);
                }
                Encoding::Aac(aac) => aac.encode(chunk, &mut each)?,
            }
            start += samples;
        }
        self.pending.drain(..start);
        Ok(())
    }
}

enum Decoding {
    Opus(opus::Decoder),
    Aac {
        aac: aac::AacDecoder,
        converter: Option<Box<Converter>>,
        decoded: Vec<f32>,
    },
}

/// Claude: decodes one stream's packets to interleaved f32 at `DECODED_RATE`,
/// keeping the stream's channel count.
pub struct Decoder {
    codec: Codec,
    decoding: Decoding,
    channels: usize,
    pcm: Vec<f32>,
}

impl Decoder {
    pub fn new(codec: Codec, format: StreamFormat) -> Result<Self, String> {
        let channels = usize::from(format.channels);
        if channels == 0 {
            return Err(format!("{codec} stream declares no channels"));
        }
        let decoding = match codec {
            Codec::Opus => {
                let layout = match channels {
                    1 => Channels::Mono,
                    2 => Channels::Stereo,
                    n => return Err(format!("{codec} streams of {n} channels are not supported")),
                };
                Decoding::Opus(
                    opus::Decoder::new(DECODED_RATE, layout)
                        .map_err(|e| format!("create the {codec} decoder: {e}"))?,
                )
            }
            Codec::Aac => Decoding::Aac {
                aac: aac::AacDecoder::new(format)?,
                converter: (format.sample_rate != DECODED_RATE)
                    .then(|| {
                        Converter::new(
                            f64::from(format.sample_rate),
                            f64::from(DECODED_RATE),
                            channels,
                            codec.frames_per_packet() as usize,
                            false,
                        )
                        .map(Box::new)
                    })
                    .transpose()?,
                decoded: Vec::new(),
            },
        };
        Ok(Self {
            codec,
            decoding,
            channels,
            pcm: vec![0.0; MAX_DECODED_FRAMES * channels],
        })
    }

    /// Claude: appends the interleaved frames `packet` decodes to and returns how
    /// many. An empty packet stands in for a lost one: Opus conceals it with a
    /// packet's worth of frames, AAC yields none.
    pub fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<usize, String> {
        let before = out.len();
        match &mut self.decoding {
            Decoding::Opus(opus) => {
                let room = if packet.is_empty() {
                    self.codec.frames_per_packet() as usize
                } else {
                    MAX_DECODED_FRAMES
                };
                let frames = opus
                    .decode_float(packet, &mut self.pcm[..room * self.channels], false)
                    .map_err(|e| format!("{} decode failed: {e}", self.codec))?;
                out.extend_from_slice(&self.pcm[..frames * self.channels]);
            }
            Decoding::Aac { .. } if packet.is_empty() => {}
            Decoding::Aac {
                aac,
                converter,
                decoded,
            } => match converter {
                Some(converter) => {
                    decoded.clear();
                    aac.decode(packet, decoded)?;
                    converter.push(decoded, out)?;
                }
                None => aac.decode(packet, out)?,
            },
        }
        Ok((out.len() - before) / self.channels)
    }
}
