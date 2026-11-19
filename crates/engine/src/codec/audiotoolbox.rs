use coreaudio_sys::*;

use super::{Codec, DECODED_RATE, StreamFormat};
use crate::abl;
use crate::ring::CHANNELS;

/// Claude: returned by the input callbacks once their packet is consumed. The
/// converter stops filling and hands the status back to `AudioConverterFillComplexBuffer`.
const DRAINED: OSStatus = 0x64726e64;

/// Claude: the rates each encoder takes. AudioToolbox's AAC-LC encoder refuses
/// 64, 88.2 and 96 kHz, and its Opus encoder refuses 44.1 kHz.
pub(super) fn encode_rates(codec: Codec) -> &'static [u32] {
    match codec {
        Codec::Aac => &[48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000],
        Codec::Opus => &[48000],
    }
}

fn description(codec: Codec, format: StreamFormat) -> AudioStreamBasicDescription {
    let (format_id, format_flags) = match codec {
        Codec::Aac => (kAudioFormatMPEG4AAC, kMPEG4Object_AAC_LC),
        Codec::Opus => (kAudioFormatOpus, 0),
    };
    AudioStreamBasicDescription {
        mSampleRate: f64::from(format.sample_rate),
        mFormatID: format_id,
        mFormatFlags: format_flags,
        mBytesPerPacket: 0,
        mFramesPerPacket: codec.frames_per_packet(),
        mBytesPerFrame: 0,
        mChannelsPerFrame: u32::from(format.channels),
        mBitsPerChannel: 0,
        mReserved: 0,
    }
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
            &description(codec, format),
        )?;
        let bitrate = codec.bitrate();
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
        self.codec.frames_per_packet() as usize
    }

    /// Claude: the frames an AAC encoder primes with, which its output trails
    /// its input by: 2112, after Apple's TN2258. Opus streams count their
    /// look-ahead in `OPUS_DELAY_FRAMES` instead.
    pub fn priming_frames(&self) -> usize {
        match self.codec {
            Codec::Aac => 2112,
            Codec::Opus => 0,
        }
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
            &description(codec, format),
            &pcm_format(output_rate, u32::from(format.channels)),
        )?;
        let packet_frames = codec.frames_per_packet() as usize;
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
