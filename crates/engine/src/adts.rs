use crate::codec::StreamFormat;

const ADTS_HEADER_LEN: usize = 7;
const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

fn rate_index(format: StreamFormat) -> Option<u8> {
    SAMPLE_RATES
        .iter()
        .position(|&rate| rate == format.sample_rate)
        .map(|index| index as u8)
}

/// Claude: ADTS channel configuration 7 is eight channels.
fn channel_config(format: StreamFormat) -> Option<u8> {
    match format.channels {
        1..=6 => Some(format.channels),
        8 => Some(7),
        _ => None,
    }
}

/// Claude: an ADTS frame wrapping one AAC-LC access unit.
pub fn adts_frame(format: StreamFormat, access_unit: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
    let index = rate_index(format)
        .ok_or_else(|| format!("AAC has no sampling frequency index for {} Hz", format.sample_rate))?;
    let channels = channel_config(format)
        .ok_or_else(|| format!("ADTS cannot carry {} channels", format.channels))?;
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
/// format and access unit, and the number of bytes consumed. Bytes before a
/// sync word are skipped; an incomplete trailing frame is left unconsumed.
pub fn parse_adts(data: &[u8], mut each: impl FnMut(StreamFormat, &[u8])) -> usize {
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
            StreamFormat {
                sample_rate,
                channels,
            },
            &data[at + header_len..at + len],
        );
        at += len;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adts_frames_round_trip_their_format_and_payload() {
        for format in [
            StreamFormat { sample_rate: 48000, channels: 2 },
            StreamFormat { sample_rate: 44100, channels: 1 },
        ] {
            let mut stream = vec![0x12, 0x34];
            adts_frame(format, &[1, 2, 3], &mut stream).unwrap();
            adts_frame(format, &[9; 700], &mut stream).unwrap();
            let partial = stream.len();
            adts_frame(format, &[7; 10], &mut stream).unwrap();
            stream.truncate(partial + 5);

            let mut frames = Vec::new();
            let consumed = parse_adts(&stream, |f, unit| frames.push((f, unit.to_vec())));
            assert_eq!(consumed, partial);
            assert_eq!(frames, [(format, vec![1, 2, 3]), (format, vec![9; 700])]);
        }
        let mut out = Vec::new();
        assert!(adts_frame(StreamFormat { sample_rate: 47000, channels: 2 }, &[0], &mut out).is_err());
    }

    #[test]
    fn adts_channel_configuration_seven_is_eight_channels() {
        let format = StreamFormat { sample_rate: 48000, channels: 8 };
        let mut stream = Vec::new();
        adts_frame(format, &[1], &mut stream).unwrap();
        let mut seen = Vec::new();
        parse_adts(&stream, |f, _| seen.push(f));
        assert_eq!(seen, [format]);
        let mut out = Vec::new();
        assert!(adts_frame(StreamFormat { sample_rate: 48000, channels: 7 }, &[1], &mut out).is_err());
    }
}
