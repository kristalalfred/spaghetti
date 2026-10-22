use coreaudio_sys::*;

use crate::abl;

pub const FRAMES_PER_PACKET: usize = 1024;

const ADTS_HEADER_LEN: usize = 7;
const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];
/// Claude: the rates AudioToolbox's AAC-LC encoder accepts; it refuses 64, 88.2 and 96 kHz.
const ENCODER_RATES: [u32; 9] = [48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000];
/// Claude: returned by the input callbacks once their packet is consumed. The
/// converter stops filling and hands the status back to `AudioConverterFillComplexBuffer`.
const DRAINED: OSStatus = 0x64726e64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AacConfig {
    pub sample_rate: u32,
    pub channels: u8,
}

impl AacConfig {
    fn rate_index(self) -> Option<u8> {
        SAMPLE_RATES
            .iter()
            .position(|&rate| rate == self.sample_rate)
            .map(|index| index as u8)
    }

    /// Claude: ADTS channel configuration 7 is eight channels.
    fn channel_config(self) -> Option<u8> {
        match self.channels {
            1..=6 => Some(self.channels),
            8 => Some(7),
            _ => None,
        }
    }
}

/// Claude: the rate to encode a source at: its own when the encoder takes it,
/// otherwise 44.1 kHz for its multiples and 48 kHz for anything else.
fn encode_rate(source_rate: u32) -> u32 {
    if ENCODER_RATES.contains(&source_rate) {
        source_rate
    } else if source_rate.is_multiple_of(44100) {
        44100
    } else {
        48000
    }
}

/// Claude: an ADTS frame wrapping one AAC-LC access unit.
pub fn adts_frame(config: AacConfig, access_unit: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
    let index = config
        .rate_index()
        .ok_or_else(|| format!("AAC has no sampling frequency index for {} Hz", config.sample_rate))?;
    let channels = config
        .channel_config()
        .ok_or_else(|| format!("ADTS cannot carry {} channels", config.channels))?;
    let len = access_unit.len() + ADTS_HEADER_LEN;
    if len >= 1 << 13 {
        return Err(format!("AAC access unit of {} bytes does not fit ADTS", access_unit.len()));
    }
    let profile = 1u8;
    out.extend_from_slice(&[
        0xff,
        0xf1,
        (profile << 6) | (index << 2) | ((channels >> 2) & 1),
        ((channels & 3) << 6) | ((len >> 11) as u8 & 3),
        (len >> 3) as u8,
        ((len as u8 & 7) << 5) | 0x1f,
        0xfc,
    ]);
    out.extend_from_slice(access_unit);
    Ok(())
}

/// Claude: splits the ADTS frames at the start of `data`. Returns each frame's
/// config and access unit, and the number of bytes consumed. Bytes before a
/// sync word are skipped; an incomplete trailing frame is left unconsumed.
pub fn parse_adts(data: &[u8], mut each: impl FnMut(AacConfig, &[u8])) -> usize {
    let mut at = 0;
    while at + ADTS_HEADER_LEN <= data.len() {
        let header = &data[at..];
        if header[0] != 0xff || header[1] & 0xf0 != 0xf0 {
            at += 1;
            continue;
        }
        let protection_absent = header[1] & 1 == 1;
        let header_len = if protection_absent { 7 } else { 9 };
        let index = (header[2] >> 2) & 0xf;
        let channels = match ((header[2] & 1) << 2) | (header[3] >> 6) {
            7 => 8,
            config => config,
        };
        let len = ((header[3] as usize & 3) << 11) | ((header[4] as usize) << 3) | (header[5] as usize >> 5);
        let Some(&sample_rate) = SAMPLE_RATES.get(index as usize) else {
            at += 1;
            continue;
        };
        if len < header_len {
            at += 1;
            continue;
        }
        if at + len > data.len() {
            break;
        }
        each(
            AacConfig {
                sample_rate,
                channels,
            },
            &data[at + header_len..at + len],
        );
        at += len;
    }
    at
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

fn aac_format(config: AacConfig) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: f64::from(config.sample_rate),
        mFormatID: kAudioFormatMPEG4AAC,
        mFormatFlags: kMPEG4Object_AAC_LC,
        mBytesPerPacket: 0,
        mFramesPerPacket: FRAMES_PER_PACKET as u32,
        mBytesPerFrame: 0,
        mChannelsPerFrame: u32::from(config.channels),
        mBitsPerChannel: 0,
        mReserved: 0,
    }
}

struct Converter(AudioConverterRef);

// Claude: a converter is used by one thread at a time.
unsafe impl Send for Converter {}

impl Converter {
    fn new(from: &AudioStreamBasicDescription, to: &AudioStreamBasicDescription) -> Result<Self, String> {
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

/// Claude: AAC-LC encoder for interleaved stereo f32 at the source's rate. It
/// encodes at `encode_rate` of it, resampling in the same converter, so the
/// stream's access units each hold `FRAMES_PER_PACKET` frames at `config().sample_rate`.
pub struct AacEncoder {
    converter: Converter,
    config: AacConfig,
    packet: Vec<u8>,
}

impl AacEncoder {
    pub fn new(source_rate: u32, bitrate: u32) -> Result<Self, String> {
        let config = AacConfig {
            sample_rate: encode_rate(source_rate),
            channels: 2,
        };
        let converter =
            Converter::new(&pcm_format(f64::from(source_rate), 2), &aac_format(config))?;
        let status = unsafe {
            AudioConverterSetProperty(
                converter.0,
                kAudioConverterEncodeBitRate,
                std::mem::size_of::<u32>() as u32,
                (&bitrate as *const u32).cast(),
            )
        };
        if status != 0 {
            return Err(format!("set AAC bitrate {bitrate} failed: {status}"));
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
            return Err(format!("get AAC maximum packet size failed: {status}"));
        }
        Ok(Self {
            converter,
            config,
            packet: vec![0; max_packet as usize],
        })
    }

    pub fn config(&self) -> AacConfig {
        self.config
    }

    /// Claude: feeds interleaved stereo frames at the source rate and hands each
    /// access unit they complete to `each`, in order.
    pub fn encode(&mut self, pcm: &[f32], mut each: impl FnMut(&[u8])) -> Result<(), String> {
        let mut input = PcmInput {
            data: pcm.as_ptr(),
            frames: (pcm.len() / 2) as u32,
            channels: 2,
        };
        loop {
            let mut output = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer {
                    mNumberChannels: 2,
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
                return Err(format!("AAC encode failed: {status}"));
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
    if input.len == 0 {
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
    input.len = 0;
    0
}

/// Claude: AAC-LC decoder to interleaved f32 at `output_rate`, keeping the
/// stream's channel count.
pub struct AacDecoder {
    converter: Converter,
    config: AacConfig,
    pcm: Vec<f32>,
}

impl AacDecoder {
    pub fn new(config: AacConfig, output_rate: f64) -> Result<Self, String> {
        if config.channels == 0 {
            return Err("AAC stream declares no channels".into());
        }
        let converter = Converter::new(
            &aac_format(config),
            &pcm_format(output_rate, u32::from(config.channels)),
        )?;
        let frames = (FRAMES_PER_PACKET as f64 * output_rate / f64::from(config.sample_rate)).ceil()
            as usize
            + FRAMES_PER_PACKET;
        Ok(Self {
            converter,
            config,
            pcm: vec![0.0; frames * usize::from(config.channels)],
        })
    }

    /// Claude: appends the interleaved frames `access_unit` decodes to and returns how many.
    pub fn decode(&mut self, access_unit: &[u8], out: &mut Vec<f32>) -> Result<usize, String> {
        let channels = u32::from(self.config.channels);
        let mut input = PacketInput {
            data: access_unit.as_ptr(),
            len: access_unit.len() as u32,
            description: unsafe { std::mem::zeroed() },
            channels,
        };
        let mut output = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: channels,
                mDataByteSize: (self.pcm.len() * 4) as u32,
                mData: self.pcm.as_mut_ptr().cast(),
            }],
        };
        let mut frames = (self.pcm.len() / channels as usize) as u32;
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
            return Err(format!("AAC decode failed: {status}"));
        }
        out.extend_from_slice(&self.pcm[..frames as usize * channels as usize]);
        Ok(frames as usize)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn tone(rate: f64, hz: f64, amplitude: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|f| {
                let s = amplitude * (2.0 * std::f64::consts::PI * hz * f as f64 / rate).sin() as f32;
                [s, s]
            })
            .collect()
    }

    /// Claude: the dominant frequency by zero crossings and the RMS, of the first channel.
    pub fn measure(interleaved: &[f32], channels: usize, rate: f64) -> (f64, f32) {
        let left: Vec<f32> = interleaved.iter().step_by(channels).copied().collect();
        let crossings = left.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        let rms = (left.iter().map(|s| s * s).sum::<f32>() / left.len() as f32).sqrt();
        (crossings as f64 * rate / left.len() as f64, rms)
    }

    fn encode_tone(rate: u32, hz: f64, packets: usize) -> Vec<(AacConfig, Vec<u8>)> {
        let mut encoder = AacEncoder::new(rate, 192_000).unwrap();
        let config = encoder.config();
        let pcm = tone(f64::from(rate), hz, 0.5, packets * FRAMES_PER_PACKET);
        let mut units = Vec::new();
        for chunk in pcm.chunks(FRAMES_PER_PACKET * 2) {
            encoder
                .encode(chunk, |unit| units.push((config, unit.to_vec())))
                .unwrap();
        }
        units
    }

    #[test]
    fn adts_frames_round_trip_their_config_and_payload() {
        for config in [
            AacConfig { sample_rate: 48000, channels: 2 },
            AacConfig { sample_rate: 44100, channels: 1 },
        ] {
            let mut stream = vec![0x12, 0x34];
            adts_frame(config, &[1, 2, 3], &mut stream).unwrap();
            adts_frame(config, &[9; 700], &mut stream).unwrap();
            let partial = stream.len();
            adts_frame(config, &[7; 10], &mut stream).unwrap();
            stream.truncate(partial + 5);

            let mut frames = Vec::new();
            let consumed = parse_adts(&stream, |c, unit| frames.push((c, unit.to_vec())));
            assert_eq!(consumed, partial);
            assert_eq!(frames, [(config, vec![1, 2, 3]), (config, vec![9; 700])]);
        }
        let mut out = Vec::new();
        assert!(adts_frame(AacConfig { sample_rate: 47000, channels: 2 }, &[0], &mut out).is_err());
    }

    #[test]
    fn a_tone_survives_encoding_and_decoding() {
        let units = encode_tone(48000, 1000.0, 60);
        assert!(units.len() >= 55, "{} access units", units.len());
        let mut decoder = AacDecoder::new(units[0].0, 48000.0).unwrap();
        let mut pcm = Vec::new();
        for (_, unit) in &units {
            decoder.decode(unit, &mut pcm).unwrap();
        }
        let steady = &pcm[pcm.len() / 2..];
        let (hz, rms) = measure(steady, 2, 48000.0);
        assert!((hz - 1000.0).abs() < 5.0, "{hz} Hz");
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.02, "rms {rms}");
    }

    #[test]
    fn a_source_above_the_encoders_rates_is_encoded_at_one_it_takes() {
        for (source, encoded) in [(96000, 48000), (88200, 44100), (64000, 48000)] {
            let units = encode_tone(source, 1000.0, 120);
            assert_eq!(units[0].0.sample_rate, encoded, "{source} Hz");
            let expected = 120 * FRAMES_PER_PACKET * encoded as usize / source as usize / FRAMES_PER_PACKET;
            assert!(units.len().abs_diff(expected) <= 3, "{source} Hz: {} units", units.len());
            let mut decoder = AacDecoder::new(units[0].0, 48000.0).unwrap();
            let mut pcm = Vec::new();
            for (_, unit) in &units {
                decoder.decode(unit, &mut pcm).unwrap();
            }
            let (hz, rms) = measure(&pcm[pcm.len() / 2..], 2, 48000.0);
            assert!((hz - 1000.0).abs() < 5.0, "{source} Hz: {hz} Hz");
            assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.03, "{source} Hz: rms {rms}");
        }
    }

    #[test]
    fn adts_channel_configuration_seven_is_eight_channels() {
        let config = AacConfig { sample_rate: 48000, channels: 8 };
        let mut stream = Vec::new();
        adts_frame(config, &[1], &mut stream).unwrap();
        let mut seen = Vec::new();
        parse_adts(&stream, |c, _| seen.push(c));
        assert_eq!(seen, [config]);
        let mut out = Vec::new();
        assert!(adts_frame(AacConfig { sample_rate: 48000, channels: 7 }, &[1], &mut out).is_err());
    }

    #[test]
    fn decoding_resamples_to_the_output_rate() {
        let units = encode_tone(44100, 1000.0, 60);
        let mut decoder = AacDecoder::new(units[0].0, 48000.0).unwrap();
        let mut pcm = Vec::new();
        let mut frames = 0;
        for (_, unit) in &units {
            frames += decoder.decode(unit, &mut pcm).unwrap();
        }
        let expected = units.len() * FRAMES_PER_PACKET * 48000 / 44100;
        assert!(frames.abs_diff(expected) < 2 * FRAMES_PER_PACKET, "{frames} vs {expected}");
        let (hz, _) = measure(&pcm[pcm.len() / 2..], 2, 48000.0);
        assert!((hz - 1000.0).abs() < 5.0, "{hz} Hz");
    }
}
