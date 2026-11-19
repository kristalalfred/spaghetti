//! Claude: AAC-LC through Media Foundation's AAC encoder and decoder
//! transforms, which ship with Windows. The encoder takes 16-bit PCM at
//! 44.1 or 48 kHz and writes raw access units; the decoder takes raw access
//! units described by their AudioSpecificConfig and writes f32.

use std::cell::Cell;
use std::mem::ManuallyDrop;
use std::sync::OnceLock;

use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSample, IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_USER_DATA, MF_VERSION,
    MFAudioFormat_AAC, MFAudioFormat_Float, MFAudioFormat_PCM, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Audio, MFSTARTUP_NOSOCKET, MFStartup,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_INFO, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::core::GUID;

use super::super::StreamFormat;

/// Claude: the AAC encoder and decoder transforms, as `wmcodecdsp.h` names them.
const CLSID_AAC_MFT_ENCODER: GUID = GUID::from_u128(0x93af0c51_2275_45d2_a35b_f2ba21caed00);
const CLSID_MS_AAC_DEC_MFT: GUID = GUID::from_u128(0x32d186a7_218f_4c75_8876_dd77273a8999);

/// Claude: the bitrates Media Foundation's AAC encoder takes, in bytes per
/// second: 96, 128, 160 and 192 kb/s.
const BYTE_RATES: [u32; 4] = [12_000, 16_000, 20_000, 24_000];
/// Claude: AAC-LC, level 2, the indication the encoder's documentation pairs
/// with stereo up to 48 kHz.
const AAC_LC_LEVEL_2: u32 = 0x29;
/// Claude: an AAC-LC access unit's frames.
const UNIT_FRAMES: u64 = 1024;

fn start() -> Result<(), String> {
    thread_local!(static JOINED: Cell<bool> = const { Cell::new(false) });
    JOINED.with(|joined| {
        if !joined.replace(true) {
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        }
    });
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
    STARTED
        .get_or_init(|| {
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }
                .map_err(|e| format!("start Media Foundation: {e}"))
        })
        .clone()
}

fn failed(what: &str) -> impl Fn(windows::core::Error) -> String + '_ {
    move |e| format!("{what}: {e}")
}

fn audio_type(subtype: &GUID, format: StreamFormat, bits: u32) -> Result<IMFMediaType, String> {
    let set = failed("describe the AAC stream");
    unsafe {
        let media = MFCreateMediaType().map_err(&set)?;
        media
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
            .map_err(&set)?;
        media.SetGUID(&MF_MT_SUBTYPE, subtype).map_err(&set)?;
        media
            .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, format.sample_rate)
            .map_err(&set)?;
        media
            .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, u32::from(format.channels))
            .map_err(&set)?;
        media
            .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, bits)
            .map_err(&set)?;
        Ok(media)
    }
}

/// Claude: the AudioSpecificConfig of an AAC-LC stream, per ISO 14496-3 1.6.2.1.
fn audio_specific_config(format: StreamFormat) -> Result<[u8; 2], String> {
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    let index = RATES
        .iter()
        .position(|&rate| rate == format.sample_rate)
        .ok_or_else(|| format!("AAC has no {} Hz", format.sample_rate))? as u16;
    let config = (2u16 << 11) | (index << 7) | (u16::from(format.channels & 0xf) << 3);
    Ok(config.to_be_bytes())
}

enum Output {
    Exactly(IMFMediaType),
    Offered(GUID),
}

/// Claude: a transform fed one input sample at a time, drained after each.
/// Its input samples are stamped by the frames fed before them.
struct Transform {
    transform: IMFTransform,
    /// Claude: the size of the samples it writes into, when we hand them to it.
    allocate: Option<u32>,
    rate: u32,
    fed_frames: u64,
}

// Claude: Media Foundation's transforms are free-threaded, and a codec is
// used by one thread at a time.
unsafe impl Send for Transform {}

impl Transform {
    /// Claude: the encoder takes its output type first; the decoder takes
    /// its input type, then offers the output types it can write, of which
    /// the first of `output`'s subtype is chosen.
    fn new(class: &GUID, input: &IMFMediaType, output: Output, rate: u32) -> Result<Self, String> {
        start()?;
        let transform: IMFTransform =
            unsafe { CoCreateInstance(class, None, CLSCTX_INPROC_SERVER) }
                .map_err(failed("create the AAC transform"))?;
        let set = failed("configure the AAC transform");
        unsafe {
            match output {
                Output::Exactly(output) => {
                    transform
                        .SetOutputType(0, &output, 0)
                        .map_err(failed("set the AAC encoder's output"))?;
                    transform
                        .SetInputType(0, input, 0)
                        .map_err(failed("set the AAC encoder's input"))?;
                }
                Output::Offered(subtype) => {
                    transform
                        .SetInputType(0, input, 0)
                        .map_err(failed("set the AAC decoder's input"))?;
                    let offered = (0..)
                        .map_while(|i| transform.GetOutputAvailableType(0, i).ok())
                        .find(|offered| offered.GetGUID(&MF_MT_SUBTYPE).is_ok_and(|g| g == subtype))
                        .ok_or("the AAC decoder offers no f32 output")?;
                    transform
                        .SetOutputType(0, &offered, 0)
                        .map_err(failed("set the AAC decoder's output"))?;
                }
            }
            let info: MFT_OUTPUT_STREAM_INFO = transform.GetOutputStreamInfo(0).map_err(&set)?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(&set)?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(&set)?;
            let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            Ok(Self {
                transform,
                allocate: (!provides).then_some(info.cbSize.max(8192)),
                rate,
                fed_frames: 0,
            })
        }
    }

    fn hns(&self, frames: u64) -> i64 {
        (u128::from(frames) * 10_000_000 / u128::from(self.rate)) as i64
    }

    /// Claude: feeds `bytes`, `frames` frames long, and hands each output
    /// buffer it yields to `each`.
    fn process(
        &mut self,
        bytes: &[u8],
        frames: u64,
        mut each: impl FnMut(&[u8]),
    ) -> Result<(), String> {
        let fail = failed("run the AAC transform");
        unsafe {
            let buffer = MFCreateMemoryBuffer(bytes.len().max(1) as u32).map_err(&fail)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None).map_err(&fail)?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
            buffer.Unlock().map_err(&fail)?;
            buffer.SetCurrentLength(bytes.len() as u32).map_err(&fail)?;
            let sample = MFCreateSample().map_err(&fail)?;
            sample.AddBuffer(&buffer).map_err(&fail)?;
            let start = self.hns(self.fed_frames);
            self.fed_frames += frames;
            sample.SetSampleTime(start).map_err(&fail)?;
            sample
                .SetSampleDuration(self.hns(self.fed_frames) - start)
                .map_err(&fail)?;
            self.transform.ProcessInput(0, &sample, 0).map_err(&fail)?;
            loop {
                let provided = match self.allocate {
                    Some(size) => {
                        let sample = MFCreateSample().map_err(&fail)?;
                        sample
                            .AddBuffer(&MFCreateMemoryBuffer(size).map_err(&fail)?)
                            .map_err(&fail)?;
                        Some(sample)
                    }
                    None => None,
                };
                let mut output = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(provided),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0;
                let result = self.transform.ProcessOutput(0, &mut output, &mut status);
                let written: Option<IMFSample> = ManuallyDrop::take(&mut output[0].pSample);
                drop(ManuallyDrop::take(&mut output[0].pEvents));
                match result {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                    Err(e) => return Err(fail(e)),
                }
                let Some(written) = written else {
                    continue;
                };
                let contiguous = written.ConvertToContiguousBuffer().map_err(&fail)?;
                let mut data = std::ptr::null_mut();
                let mut len = 0u32;
                contiguous
                    .Lock(&mut data, None, Some(&mut len))
                    .map_err(&fail)?;
                each(std::slice::from_raw_parts(data, len as usize));
                contiguous.Unlock().map_err(&fail)?;
            }
        }
    }
}

/// Claude: the byte rate closest to `bitrate` the encoder takes.
fn byte_rate(bitrate: u32) -> u32 {
    *BYTE_RATES
        .iter()
        .min_by_key(|&&rate| rate.abs_diff(bitrate / 8))
        .expect("rates listed")
}

pub struct AacEncoder {
    transform: Transform,
    pcm: Vec<u8>,
    channels: usize,
}

impl AacEncoder {
    pub fn new(format: StreamFormat, bitrate: u32) -> Result<Self, String> {
        let input = audio_type(&MFAudioFormat_PCM, format, 16)?;
        let output = audio_type(&MFAudioFormat_AAC, format, 16)?;
        let set = failed("describe the AAC stream");
        unsafe {
            output
                .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, byte_rate(bitrate))
                .map_err(&set)?;
            output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).map_err(&set)?;
            output
                .SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, AAC_LC_LEVEL_2)
                .map_err(&set)?;
        }
        Ok(Self {
            transform: Transform::new(
                &CLSID_AAC_MFT_ENCODER,
                &input,
                Output::Exactly(output),
                format.sample_rate,
            )?,
            pcm: Vec::new(),
            channels: usize::from(format.channels),
        })
    }

    /// Claude: feeds interleaved f32 and hands each access unit to `each`.
    pub fn encode(&mut self, samples: &[f32], each: impl FnMut(&[u8])) -> Result<(), String> {
        self.pcm.clear();
        for &sample in samples {
            let value = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
            self.pcm.extend_from_slice(&value.to_le_bytes());
        }
        let frames = (samples.len() / self.channels) as u64;
        self.transform.process(&self.pcm, frames, each)
    }
}

pub struct AacDecoder {
    transform: Transform,
}

impl AacDecoder {
    pub fn new(format: StreamFormat) -> Result<Self, String> {
        let input = audio_type(&MFAudioFormat_AAC, format, 16)?;
        // Claude: HEAACWAVEINFO after its WAVEFORMATEX: raw payload, no
        // profile indicated, then the AudioSpecificConfig.
        let mut user_data = vec![0, 0, 0xfe, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        user_data.extend_from_slice(&audio_specific_config(format)?);
        let set = failed("describe the AAC stream");
        unsafe {
            input.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).map_err(&set)?;
            input.SetBlob(&MF_MT_USER_DATA, &user_data).map_err(&set)?;
        }
        Ok(Self {
            transform: Transform::new(
                &CLSID_MS_AAC_DEC_MFT,
                &input,
                Output::Offered(MFAudioFormat_Float),
                format.sample_rate,
            )?,
        })
    }

    /// Claude: appends the interleaved f32 one access unit decodes to.
    pub fn decode(&mut self, unit: &[u8], out: &mut Vec<f32>) -> Result<(), String> {
        self.transform.process(unit, UNIT_FRAMES, |bytes| {
            out.extend(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&b| f32::from_le_bytes(b)),
            );
        })
    }
}
