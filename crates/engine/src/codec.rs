use std::fmt;

use coreaudio_sys::*;

use crate::abl;
use crate::ring::CHANNELS;

/// Claude: every stream is decoded at this rate, so a source can report its
/// timing before its first packet says what the stream's rate is.
pub const DECODED_RATE: u32 = 48000;
/// Claude: how many frames at 48 kHz a decoded Opus packet's audio trails its
/// RTP time by, its decoded frames ending where the packet's span ends: the
/// encoder's pre-skip. AudioToolbox drops part of it from the first packet, so
/// that packet decodes short.
pub const OPUS_DELAY_FRAMES: u32 = 312;
/// Claude: returned by the input callbacks once their packet is consumed. The
/// converter stops filling and hands the status back to `AudioConverterFillComplexBuffer`.
const DRAINED: OSStatus = 0x64726e64;

/// Claude: a compressed format AudioToolbox encodes and decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Aac,
    Opus,
}

struct CodecParams {
    name: &'static str,
    format_id: AudioFormatID,
    format_flags: AudioFormatFlags,
    frames_per_packet: u32,
    bitrate: u32,
    /// Claude: the rates its encoder takes.
    encode_rates: &'static [u32],
}

const AAC: CodecParams = CodecParams {
    name: "AAC",
    format_id: kAudioFormatMPEG4AAC,
    format_flags: kMPEG4Object_AAC_LC,
    frames_per_packet: 1024,
    bitrate: 256_000,
    // Claude: AudioToolbox's AAC-LC encoder refuses 64, 88.2 and 96 kHz.
    encode_rates: &[48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000],
};

const OPUS: CodecParams = CodecParams {
    name: "Opus",
    format_id: kAudioFormatOpus,
    format_flags: 0,
    frames_per_packet: 960,
    bitrate: 128_000,
    // Claude: AudioToolbox's Opus encoder refuses 44.1 kHz.
    encode_rates: &[48000],
};

impl Codec {
    fn params(self) -> &'static CodecParams {
        match self {
            Self::Aac => &AAC,
            Self::Opus => &OPUS,
        }
    }

    /// Claude: the rate to encode a source at: its own when the encoder takes it,
    /// otherwise 44.1 kHz for its multiples when the encoder takes that, and 48 kHz
    /// for anything else.
    fn encode_rate(self, source_rate: u32) -> u32 {
        let rates = self.params().encode_rates;
        if rates.contains(&source_rate) {
            source_rate
        } else if source_rate.is_multiple_of(44100) && rates.contains(&44100) {
            44100
        } else {
            48000
        }
    }

    fn description(self, format: StreamFormat) -> AudioStreamBasicDescription {
        let params = self.params();
        AudioStreamBasicDescription {
            mSampleRate: f64::from(format.sample_rate),
            mFormatID: params.format_id,
            mFormatFlags: params.format_flags,
            mBytesPerPacket: 0,
            mFramesPerPacket: params.frames_per_packet,
            mBytesPerFrame: 0,
            mChannelsPerFrame: u32::from(format.channels),
            mBitsPerChannel: 0,
            mReserved: 0,
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.params().name)
    }
}

/// Claude: the rate and channel count of a compressed stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: u8,
}

fn pcm_format(sample_rate: f64, channels: u32) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: sample_rate,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: 4 * channels,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4 * channels,
        mChannelsPerFrame: channels,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

struct Converter(AudioConverterRef);

// Claude: a converter is used by one thread at a time.
unsafe impl Send for Converter {}

impl Converter {
    fn new(
        from: &AudioStreamBasicDescription,
        to: &AudioStreamBasicDescription,
    ) -> Result<Self, String> {
        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { AudioConverterNew(from, to, &mut converter) };
        if status != 0 || converter.is_null() {
            return Err(format!("AudioConverterNew failed: {status}"));
        }
        Ok(Self(converter))
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        unsafe { AudioConverterDispose(self.0) };
    }
}

struct PcmInput {
    data: *const f32,
    frames: u32,
    channels: u32,
}

unsafe extern "C" fn pcm_input(
    _converter: AudioConverterRef,
    io_packets: *mut UInt32,
    io_data: *mut AudioBufferList,
    _descriptions: *mut *mut AudioStreamPacketDescription,
    user_data: *mut std::ffi::c_void,
) -> OSStatus {
    let input = unsafe { &mut *(user_data as *mut PcmInput) };
    if input.frames == 0 {
        unsafe { *io_packets = 0 };
        return DRAINED;
    }
    let buffer = &mut unsafe { abl::buffers_mut(io_data) }[0];
    buffer.mNumberChannels = input.channels;
    buffer.mDataByteSize = input.frames * 4 * input.channels;
    buffer.mData = input.data as *mut _;
    unsafe { *io_packets = input.frames };
    input.frames = 0;
    0
}

/// Claude: encodes interleaved stereo f32 at a source's rate into packets of
/// `frames_per_packet` frames at `format().sample_rate`, the rate
/// `Codec::encode_rate` picks for the source. The converter resamples between them.
pub struct Encoder {
    codec: Codec,
    converter: Converter,
    format: StreamFormat,
    packet: Vec<u8>,
}

impl Encoder {
    pub fn new(codec: Codec, source_rate: u32) -> Result<Self, String> {
        let format = StreamFormat {
            sample_rate: codec.encode_rate(source_rate),
            channels: CHANNELS as u8,
        };
        let converter = Converter::new(
            &pcm_format(f64::from(source_rate), CHANNELS as u32),
            &codec.description(format),
        )?;
        let bitrate = codec.params().bitrate;
        let status = unsafe {
            AudioConverterSetProperty(
                converter.0,
                kAudioConverterEncodeBitRate,
                std::mem::size_of::<u32>() as u32,
                (&bitrate as *const u32).cast(),
            )
        };
        if status != 0 {
            return Err(format!("set {codec} bitrate {bitrate} failed: {status}"));
        }
        let mut max_packet: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let status = unsafe {
            AudioConverterGetProperty(
                converter.0,
                kAudioConverterPropertyMaximumOutputPacketSize,
                &mut size,
                (&mut max_packet as *mut u32).cast(),
            )
        };
        if status != 0 || max_packet == 0 {
            return Err(format!("get {codec} maximum packet size failed: {status}"));
        }
        Ok(Self {
            codec,
            converter,
            format,
            packet: vec![0; max_packet as usize],
        })
    }

    pub fn format(&self) -> StreamFormat {
        self.format
    }

    pub fn frames_per_packet(&self) -> usize {
        self.codec.params().frames_per_packet as usize
    }

    /// Claude: feeds interleaved stereo frames at the source rate and hands each
    /// packet they complete to `each`, in order.
    pub fn encode(&mut self, pcm: &[f32], mut each: impl FnMut(&[u8])) -> Result<(), String> {
        let mut input = PcmInput {
            data: pcm.as_ptr(),
            frames: (pcm.len() / CHANNELS) as u32,
            channels: CHANNELS as u32,
        };
        loop {
            let mut output = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer {
                    mNumberChannels: CHANNELS as u32,
                    mDataByteSize: self.packet.len() as u32,
                    mData: self.packet.as_mut_ptr().cast(),
                }],
            };
            let mut description: AudioStreamPacketDescription = unsafe { std::mem::zeroed() };
            let mut packets: u32 = 1;
            let status = unsafe {
                AudioConverterFillComplexBuffer(
                    self.converter.0,
                    Some(pcm_input),
                    (&mut input as *mut PcmInput).cast(),
                    &mut packets,
                    &mut output,
                    &mut description,
                )
            };
            if status != 0 && status != DRAINED {
                return Err(format!("{} encode failed: {status}", self.codec));
            }
            if packets == 0 {
                return Ok(());
            }
            let start = description.mStartOffset as usize;
            each(&self.packet[start..start + description.mDataByteSize as usize]);
        }
    }
}

struct PacketInput {
    data: *const u8,
    len: u32,
    delivered: bool,
    description: AudioStreamPacketDescription,
    channels: u32,
}

unsafe extern "C" fn packet_input(
    _converter: AudioConverterRef,
    io_packets: *mut UInt32,
    io_data: *mut AudioBufferList,
    descriptions: *mut *mut AudioStreamPacketDescription,
    user_data: *mut std::ffi::c_void,
) -> OSStatus {
    let input = unsafe { &mut *(user_data as *mut PacketInput) };
    if input.delivered {
        unsafe { *io_packets = 0 };
        return DRAINED;
    }
    let buffer = &mut unsafe { abl::buffers_mut(io_data) }[0];
    buffer.mNumberChannels = input.channels;
    buffer.mDataByteSize = input.len;
    buffer.mData = input.data as *mut _;
    input.description = AudioStreamPacketDescription {
        mStartOffset: 0,
        mVariableFramesInPacket: 0,
        mDataByteSize: input.len,
    };
    if !descriptions.is_null() {
        unsafe { *descriptions = &mut input.description };
    }
    unsafe { *io_packets = 1 };
    input.delivered = true;
    0
}

/// Claude: decodes one stream's packets to interleaved f32 at `DECODED_RATE`,
/// keeping the stream's channel count.
pub struct Decoder {
    codec: Codec,
    converter: Converter,
    channels: u32,
    pcm: Vec<f32>,
}

impl Decoder {
    pub fn new(codec: Codec, format: StreamFormat) -> Result<Self, String> {
        if format.channels == 0 {
            return Err(format!("{codec} stream declares no channels"));
        }
        let output_rate = f64::from(DECODED_RATE);
        let converter = Converter::new(
            &codec.description(format),
            &pcm_format(output_rate, u32::from(format.channels)),
        )?;
        let packet_frames = codec.params().frames_per_packet as usize;
        let frames = (packet_frames as f64 * output_rate / f64::from(format.sample_rate)).ceil()
            as usize
            + packet_frames;
        Ok(Self {
            codec,
            converter,
            channels: u32::from(format.channels),
            pcm: vec![0.0; frames * usize::from(format.channels)],
        })
    }

    /// Claude: appends the interleaved frames `packet` decodes to and returns how
    /// many. An empty packet stands in for a lost one: Opus conceals it with a
    /// packet's worth of frames, AAC yields none.
    pub fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<usize, String> {
        let mut input = PacketInput {
            data: packet.as_ptr(),
            len: packet.len() as u32,
            delivered: false,
            description: unsafe { std::mem::zeroed() },
            channels: self.channels,
        };
        let mut output = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: self.channels,
                mDataByteSize: (self.pcm.len() * 4) as u32,
                mData: self.pcm.as_mut_ptr().cast(),
            }],
        };
        let mut frames = (self.pcm.len() / self.channels as usize) as u32;
        let status = unsafe {
            AudioConverterFillComplexBuffer(
                self.converter.0,
                Some(packet_input),
                (&mut input as *mut PacketInput).cast(),
                &mut frames,
                &mut output,
                std::ptr::null_mut(),
            )
        };
        if status != 0 && status != DRAINED {
            return Err(format!("{} decode failed: {status}", self.codec));
        }
        out.extend_from_slice(&self.pcm[..frames as usize * self.channels as usize]);
        Ok(frames as usize)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn tone(rate: f64, hz: f64, amplitude: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|f| {
                let s =
                    amplitude * (2.0 * std::f64::consts::PI * hz * f as f64 / rate).sin() as f32;
                [s, s]
            })
            .collect()
    }

    /// Claude: the dominant frequency by zero crossings and the RMS, of the first channel.
    pub fn measure(interleaved: &[f32], channels: usize, rate: f64) -> (f64, f32) {
        let left: Vec<f32> = interleaved.iter().step_by(channels).copied().collect();
        let crossings = left
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        let rms = (left.iter().map(|s| s * s).sum::<f32>() / left.len() as f32).sqrt();
        (crossings as f64 * rate / left.len() as f64, rms)
    }

    fn encode_tone(
        codec: Codec,
        rate: u32,
        hz: f64,
        packets: usize,
    ) -> (StreamFormat, Vec<Vec<u8>>) {
        let mut encoder = Encoder::new(codec, rate).unwrap();
        let chunk = encoder.frames_per_packet();
        let pcm = tone(f64::from(rate), hz, 0.5, packets * chunk);
        let mut units = Vec::new();
        for chunk in pcm.chunks(chunk * CHANNELS) {
            encoder
                .encode(chunk, |unit| units.push(unit.to_vec()))
                .unwrap();
        }
        (encoder.format(), units)
    }

    fn decode_all(codec: Codec, format: StreamFormat, units: &[Vec<u8>]) -> (Vec<f32>, usize) {
        let mut decoder = Decoder::new(codec, format).unwrap();
        let mut pcm = Vec::new();
        let mut frames = 0;
        for unit in units {
            frames += decoder.decode(unit, &mut pcm).unwrap();
        }
        (pcm, frames)
    }

    #[test]
    fn an_aac_tone_survives_encoding_and_decoding() {
        let (format, units) = encode_tone(Codec::Aac, 48000, 1000.0, 60);
        assert!(units.len() >= 55, "{} access units", units.len());
        let (pcm, _) = decode_all(Codec::Aac, format, &units);
        let steady = &pcm[pcm.len() / 2..];
        let (hz, rms) = measure(steady, 2, 48000.0);
        assert!((hz - 1000.0).abs() < 5.0, "{hz} Hz");
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.02, "rms {rms}");
    }

    #[test]
    fn a_source_above_the_aac_encoders_rates_is_encoded_at_one_it_takes() {
        for (source, encoded) in [(96000, 48000), (88200, 44100), (64000, 48000)] {
            let (format, units) = encode_tone(Codec::Aac, source, 1000.0, 120);
            assert_eq!(format.sample_rate, encoded, "{source} Hz");
            let expected = 120 * encoded as usize / source as usize;
            assert!(
                units.len().abs_diff(expected) <= 3,
                "{source} Hz: {} units",
                units.len()
            );
            let (pcm, _) = decode_all(Codec::Aac, format, &units);
            let (hz, rms) = measure(&pcm[pcm.len() / 2..], 2, 48000.0);
            assert!((hz - 1000.0).abs() < 5.0, "{source} Hz: {hz} Hz");
            assert!(
                (rms - 0.5 / 2f32.sqrt()).abs() < 0.03,
                "{source} Hz: rms {rms}"
            );
        }
    }

    fn noise(frames: usize) -> Vec<f32> {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        (0..frames * CHANNELS)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed >> 40) as f32 / (1u64 << 23) as f32 * 1.8 - 0.9
            })
            .collect()
    }

    #[test]
    fn an_opus_tone_survives_encoding_and_decoding_from_44_1_48_and_96_khz() {
        for source in [44100, 48000, 96000] {
            let (format, packets) = encode_tone(Codec::Opus, source, 1000.0, 100);
            assert_eq!(
                format,
                StreamFormat {
                    sample_rate: 48000,
                    channels: 2
                },
                "{source} Hz"
            );
            let (pcm, _) = decode_all(Codec::Opus, format, &packets);
            let (hz, rms) = measure(&pcm[pcm.len() / 2..], 2, 48000.0);
            assert!((hz - 1000.0).abs() < 5.0, "{source} Hz: {hz} Hz");
            assert!(
                (rms - 0.5 / 2f32.sqrt()).abs() < 0.02,
                "{source} Hz: rms {rms}"
            );
        }
    }

    #[test]
    fn opus_packets_hold_20_ms_each_and_stay_under_1150_bytes() {
        let mut encoder = Encoder::new(Codec::Opus, 48000).unwrap();
        assert_eq!(encoder.frames_per_packet(), 960);
        let mut packets = Vec::new();
        for chunk in noise(48000 * 4).chunks(960 * CHANNELS) {
            encoder
                .encode(chunk, |packet| packets.push(packet.to_vec()))
                .unwrap();
        }
        assert!(packets.len() >= 195, "{} packets", packets.len());
        let largest = packets.iter().map(Vec::len).max().unwrap();
        assert!(largest < 1150, "{largest} bytes");

        let mut decoder = Decoder::new(Codec::Opus, encoder.format()).unwrap();
        let mut pcm = Vec::new();
        let frames: Vec<usize> = packets
            .iter()
            .map(|p| decoder.decode(p, &mut pcm).unwrap())
            .collect();
        // Claude: the decoder holds back part of the first packet as its delay.
        assert!(frames[1..].iter().all(|&f| f == 960), "{frames:?}");
    }

    #[test]
    fn a_lost_opus_packet_leaves_a_gap_unless_an_empty_packet_stands_in() {
        let (format, packets) = encode_tone(Codec::Opus, 48000, 1000.0, 50);
        let lost = 25;

        let mut decoder = Decoder::new(Codec::Opus, format).unwrap();
        let mut skipped = Vec::new();
        for (i, packet) in packets.iter().enumerate().filter(|&(i, _)| i != lost) {
            let frames = decoder.decode(packet, &mut skipped).unwrap();
            assert!(i < lost || frames == 960, "packet {i}: {frames} frames");
        }

        let mut decoder = Decoder::new(Codec::Opus, format).unwrap();
        let mut concealed = Vec::new();
        let mut gap = 0..0;
        for (i, packet) in packets.iter().enumerate() {
            let start = concealed.len();
            if i == lost {
                decoder.decode(&[], &mut concealed).unwrap();
                gap = start..concealed.len();
            } else {
                decoder.decode(packet, &mut concealed).unwrap();
            }
        }
        assert_eq!(gap.len(), 960 * CHANNELS);
        assert_eq!(concealed.len(), skipped.len() + gap.len());
        let (hz, rms) = measure(&concealed[gap], CHANNELS, 48000.0);
        assert!((hz - 1000.0).abs() < 100.0, "{hz} Hz");
        assert!(rms > 0.5 / 2f32.sqrt() / 2.0, "rms {rms}");
    }

    #[test]
    fn an_empty_aac_packet_decodes_to_nothing() {
        let (format, units) = encode_tone(Codec::Aac, 48000, 1000.0, 4);
        let mut decoder = Decoder::new(Codec::Aac, format).unwrap();
        let mut pcm = Vec::new();
        decoder.decode(&units[0], &mut pcm).unwrap();
        assert_eq!(decoder.decode(&[], &mut pcm), Ok(0));
        assert_eq!(decoder.decode(&units[1], &mut pcm), Ok(1024));
    }

    #[test]
    fn aac_decoding_resamples_to_the_decoded_rate() {
        let (format, units) = encode_tone(Codec::Aac, 44100, 1000.0, 60);
        let (pcm, frames) = decode_all(Codec::Aac, format, &units);
        let expected = units.len() * 1024 * 48000 / 44100;
        assert!(
            frames.abs_diff(expected) < 2 * 1024,
            "{frames} vs {expected}"
        );
        let (hz, _) = measure(&pcm[pcm.len() / 2..], 2, 48000.0);
        assert!((hz - 1000.0).abs() < 5.0, "{hz} Hz");
    }
    #[test]
    fn opus_audio_trails_its_rtp_time_by_its_known_frames() {
        let mut encoder = Encoder::new(Codec::Opus, 48000).unwrap();
        let chunk = encoder.frames_per_packet();
        let onset = chunk * 5 + 123;
        let total = chunk * 20;
        let sweep = |f: usize| {
            let t = f.saturating_sub(onset) as f64 / 48000.0;
            let phase = 2.0 * std::f64::consts::PI * (700.0 * t + 2300.0 * t * t);
            if f < onset {
                0.0
            } else {
                0.5 * phase.sin() as f32
            }
        };
        let pcm: Vec<f32> = (0..total).flat_map(|f| [sweep(f); CHANNELS]).collect();
        let mut units = Vec::new();
        for chunk in pcm.chunks(chunk * CHANNELS) {
            encoder
                .encode(chunk, |unit| units.push(unit.to_vec()))
                .unwrap();
        }
        let mut decoder = Decoder::new(Codec::Opus, encoder.format()).unwrap();
        let mut by_rtp_time = vec![0.0f32; total];
        for (packet, unit) in units.iter().enumerate() {
            let mut decoded = Vec::new();
            let frames = decoder.decode(unit, &mut decoded).unwrap();
            let start = (packet + 1) * chunk - frames;
            for (frame, samples) in decoded.chunks(CHANNELS).enumerate() {
                by_rtp_time[start + frame] = samples[0];
            }
        }
        let correlation = |delay: usize| -> f32 {
            (onset..onset + 4000)
                .map(|f| sweep(f) * by_rtp_time.get(f + delay).copied().unwrap_or(0.0))
                .sum()
        };
        let delay = (0..1500)
            .max_by(|&a, &b| correlation(a).total_cmp(&correlation(b)))
            .unwrap();
        assert!(
            delay.abs_diff(OPUS_DELAY_FRAMES as usize) <= 1,
            "Opus audio trails its RTP time by {delay} frames"
        );
    }
}
