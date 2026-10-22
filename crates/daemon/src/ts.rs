pub const PACKET_LEN: usize = 188;

const SYNC: u8 = 0x47;
const PAT_PID: u16 = 0;
const PMT_PID: u16 = 0x1000;
const AUDIO_PID: u16 = 0x100;
const STREAM_TYPE_ADTS: u8 = 0x0f;
const AUDIO_STREAM_ID: u8 = 0xc0;
const PSI_EVERY: u32 = 10;
/// Claude: 100 ms of 90 kHz ticks between the PCR and the PTS it precedes.
const PTS_LEAD: u64 = 9000;

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Claude: muxes one ADTS AAC stream into MPEG-TS: a PAT and PMT every
/// `PSI_EVERY` frames and one PES per ADTS frame, each carrying a PCR.
pub struct TsMuxer {
    continuity: [u8; 3],
    frames: u32,
}

impl TsMuxer {
    pub fn new() -> Self {
        Self {
            continuity: [0; 3],
            frames: 0,
        }
    }

    /// Claude: `time_90k` is the frame's media time in 90 kHz ticks. Appends
    /// whole 188-byte packets to `out`.
    pub fn mux(&mut self, adts: &[u8], time_90k: u64, out: &mut Vec<u8>) {
        if self.frames.is_multiple_of(PSI_EVERY) {
            self.section(PAT_PID, &pat(), out);
            self.section(PMT_PID, &pmt(), out);
        }
        self.frames = self.frames.wrapping_add(1);

        let pts = time_90k + PTS_LEAD;
        let mut pes = Vec::with_capacity(adts.len() + 14);
        pes.extend_from_slice(&[0, 0, 1, AUDIO_STREAM_ID]);
        let pes_len = (adts.len() + 8).min(0xffff) as u16;
        pes.extend_from_slice(&pes_len.to_be_bytes());
        pes.extend_from_slice(&[0x80, 0x80, 5]);
        pes.extend_from_slice(&timestamp(0x2, pts));
        pes.extend_from_slice(adts);

        let mut rest = &pes[..];
        let mut first = true;
        while !rest.is_empty() {
            let mut adaptation = first.then(|| {
                let mut field = vec![0x10];
                field.extend_from_slice(&pcr(time_90k));
                field
            });
            let overhead = adaptation.as_ref().map_or(0, |field| 1 + field.len());
            let room = PACKET_LEN - 4 - overhead;
            let take = rest.len().min(room);
            let mut stuffing = room - take;
            if stuffing > 0 {
                let field = adaptation.get_or_insert_with(Vec::new);
                if overhead == 0 {
                    stuffing -= 1;
                    if stuffing > 0 {
                        field.push(0);
                        stuffing -= 1;
                    }
                }
                field.resize(field.len() + stuffing, 0xff);
            }
            let (payload, remaining) = rest.split_at(take);
            self.packet(AUDIO_PID, first, adaptation.as_deref(), payload, out);
            rest = remaining;
            first = false;
        }
    }

    fn section(&mut self, pid: u16, section: &[u8], out: &mut Vec<u8>) {
        let mut payload = vec![0];
        payload.extend_from_slice(section);
        payload.resize(PACKET_LEN - 4, 0xff);
        self.packet(pid, true, None, &payload, out);
    }

    fn packet(
        &mut self,
        pid: u16,
        unit_start: bool,
        adaptation: Option<&[u8]>,
        payload: &[u8],
        out: &mut Vec<u8>,
    ) {
        let counter = match pid {
            PAT_PID => &mut self.continuity[0],
            PMT_PID => &mut self.continuity[1],
            _ => &mut self.continuity[2],
        };
        let start = out.len();
        out.extend_from_slice(&[
            SYNC,
            (u8::from(unit_start) << 6) | (pid >> 8) as u8,
            pid as u8,
            (if adaptation.is_some() { 0x30 } else { 0x10 }) | *counter,
        ]);
        *counter = (*counter + 1) & 0xf;
        if let Some(field) = adaptation {
            out.push(field.len() as u8);
            out.extend_from_slice(field);
        }
        out.extend_from_slice(payload);
        debug_assert_eq!(out.len() - start, PACKET_LEN);
    }
}

fn pat() -> Vec<u8> {
    let mut section = vec![0x00, 0xb0, 13, 0x00, 0x01, 0xc1, 0x00, 0x00, 0x00, 0x01];
    section.extend_from_slice(&[0xe0 | (PMT_PID >> 8) as u8, PMT_PID as u8]);
    let crc = crc32(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

fn pmt() -> Vec<u8> {
    let mut section = vec![0x02, 0xb0, 18, 0x00, 0x01, 0xc1, 0x00, 0x00];
    section.extend_from_slice(&[0xe0 | (AUDIO_PID >> 8) as u8, AUDIO_PID as u8, 0xf0, 0x00]);
    section.extend_from_slice(&[
        STREAM_TYPE_ADTS,
        0xe0 | (AUDIO_PID >> 8) as u8,
        AUDIO_PID as u8,
        0xf0,
        0x00,
    ]);
    let crc = crc32(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

fn timestamp(prefix: u8, ts: u64) -> [u8; 5] {
    let ts = ts & 0x1_ffff_ffff;
    [
        (prefix << 4) | (((ts >> 30) as u8 & 0x7) << 1) | 1,
        (ts >> 22) as u8,
        (((ts >> 15) as u8 & 0x7f) << 1) | 1,
        (ts >> 7) as u8,
        ((ts as u8 & 0x7f) << 1) | 1,
    ]
}

fn pcr(time_90k: u64) -> [u8; 6] {
    let base = time_90k & 0x1_ffff_ffff;
    [
        (base >> 25) as u8,
        (base >> 17) as u8,
        (base >> 9) as u8,
        (base >> 1) as u8,
        ((base as u8 & 1) << 7) | 0x7e,
        0,
    ]
}

fn read_timestamp(bytes: &[u8]) -> u64 {
    (u64::from(bytes[0] >> 1) & 0x7) << 30
        | u64::from(bytes[1]) << 22
        | u64::from(bytes[2] >> 1) << 15
        | u64::from(bytes[3]) << 7
        | u64::from(bytes[4] >> 1)
}

/// Claude: finds the first ADTS AAC stream through the PAT and PMT and hands
/// out each complete PES payload with its PTS. Input may be split anywhere.
pub struct TsDemuxer {
    partial: Vec<u8>,
    pmt_pid: Option<u16>,
    audio_pid: Option<u16>,
    pes: Vec<u8>,
}

impl TsDemuxer {
    pub fn new() -> Self {
        Self {
            partial: Vec::with_capacity(PACKET_LEN),
            pmt_pid: None,
            audio_pid: None,
            pes: Vec::new(),
        }
    }

    pub fn push(&mut self, mut bytes: &[u8], mut each: impl FnMut(&[u8], Option<u64>)) {
        if !self.partial.is_empty() {
            let take = (PACKET_LEN - self.partial.len()).min(bytes.len());
            self.partial.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.partial.len() < PACKET_LEN {
                return;
            }
            let packet = std::mem::take(&mut self.partial);
            self.packet(&packet, &mut each);
            self.partial = packet;
            self.partial.clear();
        }
        while !bytes.is_empty() {
            if bytes[0] != SYNC {
                bytes = &bytes[1..];
                continue;
            }
            if bytes.len() < PACKET_LEN {
                self.partial.extend_from_slice(bytes);
                return;
            }
            let (packet, rest) = bytes.split_at(PACKET_LEN);
            self.packet(packet, &mut each);
            bytes = rest;
        }
    }

    fn packet(&mut self, packet: &[u8], each: &mut impl FnMut(&[u8], Option<u64>)) {
        let unit_start = packet[1] & 0x40 != 0;
        let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
        let control = (packet[3] >> 4) & 0x3;
        let mut at = 4;
        if control & 0x2 != 0 {
            at += 1 + packet[4] as usize;
        }
        if control & 0x1 == 0 || at >= PACKET_LEN {
            return;
        }
        let payload = &packet[at..];

        if pid == PAT_PID && unit_start {
            self.pmt_pid = psi_section(payload, 0x00).and_then(pat_pmt_pid);
        } else if Some(pid) == self.pmt_pid && unit_start {
            self.audio_pid = psi_section(payload, 0x02).and_then(pmt_adts_pid);
        } else if Some(pid) == self.audio_pid {
            if unit_start {
                self.finish_pes(each);
                self.pes.clear();
            }
            self.pes.extend_from_slice(payload);
            if self.pes_complete() {
                self.finish_pes(each);
                self.pes.clear();
            }
        }
    }

    fn pes_complete(&self) -> bool {
        self.pes.len() >= 6 && {
            let len = usize::from(u16::from_be_bytes([self.pes[4], self.pes[5]]));
            len != 0 && self.pes.len() >= 6 + len
        }
    }

    fn finish_pes(&self, each: &mut impl FnMut(&[u8], Option<u64>)) {
        let pes = &self.pes;
        if pes.len() < 9 || pes[..3] != [0, 0, 1] {
            return;
        }
        let header_len = 9 + pes[8] as usize;
        if pes.len() < header_len {
            return;
        }
        let pts = (pes[7] & 0x80 != 0 && pes.len() >= 14).then(|| read_timestamp(&pes[9..14]));
        let declared = usize::from(u16::from_be_bytes([pes[4], pes[5]]));
        let end = if declared == 0 {
            pes.len()
        } else {
            (6 + declared).min(pes.len())
        };
        each(&pes[header_len..end], pts);
    }
}

fn psi_section(payload: &[u8], table_id: u8) -> Option<&[u8]> {
    let pointer = *payload.first()? as usize;
    let section = payload.get(1 + pointer..)?;
    if *section.first()? != table_id || section.len() < 3 {
        return None;
    }
    let len = (usize::from(section[1] & 0x0f) << 8) | usize::from(section[2]);
    section.get(..3 + len)
}

fn pat_pmt_pid(section: &[u8]) -> Option<u16> {
    let programs = section.get(8..section.len().checked_sub(4)?)?;
    programs
        .as_chunks::<4>()
        .0
        .iter()
        .find(|program| u16::from_be_bytes([program[0], program[1]]) != 0)
        .map(|program| (u16::from(program[2] & 0x1f) << 8) | u16::from(program[3]))
}

fn pmt_adts_pid(section: &[u8]) -> Option<u16> {
    let info_len = (usize::from(*section.get(10)? & 0x0f) << 8) | usize::from(*section.get(11)?);
    let mut streams = section.get(12 + info_len..section.len().checked_sub(4)?)?;
    while streams.len() >= 5 {
        let pid = (u16::from(streams[1] & 0x1f) << 8) | u16::from(streams[2]);
        let es_len = (usize::from(streams[3] & 0x0f) << 8) | usize::from(streams[4]);
        if streams[0] == STREAM_TYPE_ADTS {
            return Some(pid);
        }
        streams = streams.get(5 + es_len..)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aac::{self, AacConfig, AacDecoder};

    fn adts(n: usize, len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let unit: Vec<u8> = (0..len).map(|i| (i + n) as u8).collect();
        aac::adts_frame(AacConfig { sample_rate: 48000, channels: 2 }, &unit, &mut out).unwrap();
        out
    }

    #[test]
    fn muxed_frames_demux_to_the_same_payloads_and_times_however_the_bytes_split() {
        let frames: Vec<Vec<u8>> = (0..25).map(|n| adts(n, [1, 170, 171, 172, 600, 1400][n % 6])).collect();
        let mut muxer = TsMuxer::new();
        let mut stream = Vec::new();
        for (n, frame) in frames.iter().enumerate() {
            let before = stream.len();
            muxer.mux(frame, n as u64 * 1920, &mut stream);
            assert_eq!((stream.len() - before) % PACKET_LEN, 0);
        }

        for split in [1316, 1, 187, 500] {
            let mut demuxer = TsDemuxer::new();
            let mut seen = Vec::new();
            for chunk in stream.chunks(split) {
                demuxer.push(chunk, |payload, pts| seen.push((payload.to_vec(), pts)));
            }
            assert_eq!(seen.len(), frames.len(), "split {split}");
            for (n, (payload, pts)) in seen.iter().enumerate() {
                assert_eq!(payload, &frames[n], "frame {n} split {split}");
                assert_eq!(*pts, Some(n as u64 * 1920 + PTS_LEAD));
            }
        }
    }

    #[test]
    fn the_psi_tables_carry_a_valid_crc() {
        for section in [pat(), pmt()] {
            assert_eq!(crc32(&section), 0, "a section with its CRC appended sums to zero");
        }
    }

    #[test]
    fn an_ffmpeg_stream_demuxes_and_decodes_to_its_tone() {
        let stream = include_bytes!("../testdata/tone-1khz-48k-mono.ts");
        let mut demuxer = TsDemuxer::new();
        let mut decoder: Option<AacDecoder> = None;
        let mut pcm = Vec::new();
        let mut carry = Vec::new();
        demuxer.push(stream, |payload, _| {
            carry.extend_from_slice(payload);
            let used = aac::parse_adts(&carry, |config, unit| {
                let decoder = decoder.get_or_insert_with(|| AacDecoder::new(config, 48000.0).unwrap());
                decoder.decode(unit, &mut pcm).unwrap();
            });
            carry.drain(..used);
        });
        let decoder = decoder.expect("found an ADTS stream");
        assert_eq!(decoder.config(), AacConfig { sample_rate: 48000, channels: 1 });
        let (hz, _) = aac::tests::measure(&pcm[pcm.len() / 4..pcm.len() * 3 / 4], 1, 48000.0);
        assert!((hz - 1000.0).abs() < 5.0, "{hz} Hz");
    }
}
