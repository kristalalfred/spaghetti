use crate::FRAMES_PER_PACKET;
use crate::clock::Ntp;
use crate::hap::{Counted, TAG_LEN};

pub(crate) const RTP_HEADER_LEN: usize = 12;
const ALAC_HEADER_BITS: usize = 3 + 4 + 12 + 1 + 2 + 1;
const ALAC_COUNT_BITS: usize = 32;
const ALAC_END_BITS: usize = 3;
const NONCE_LEN: usize = 8;
const RETRANSMISSION_HEADER_LEN: usize = 4;
pub(crate) const AIRPLAY1_PACKET_LEN: usize =
    RTP_HEADER_LEN + alac_frame_len(FRAMES_PER_PACKET, Framing::Counted);
pub(crate) const AIRPLAY2_PACKET_LEN: usize =
    RTP_HEADER_LEN + alac_frame_len(FRAMES_PER_PACKET, Framing::Full) + TAG_LEN + NONCE_LEN;
pub(crate) const MAX_AUDIO_PACKET_LEN: usize = if AIRPLAY1_PACKET_LEN > AIRPLAY2_PACKET_LEN {
    AIRPLAY1_PACKET_LEN
} else {
    AIRPLAY2_PACKET_LEN
};
pub(crate) const MAX_RETRANSMISSION_LEN: usize = RETRANSMISSION_HEADER_LEN + MAX_AUDIO_PACKET_LEN;
const BUFFERED_LENGTH_LEN: usize = 2;
pub(crate) const BUFFERED_PACKET_LEN: usize = BUFFERED_LENGTH_LEN + AIRPLAY2_PACKET_LEN;
/// Claude: how many sequence numbers a buffered packet's header holds.
pub(crate) const BUFFERED_SEQUENCES: u32 = 1 << 23;
const IPV6_HEADER_LEN: usize = 40;
const UDP_HEADER_LEN: usize = 8;
const ETHERNET_MTU: usize = 1500;
/// Claude: receivers resolve to link-local IPv6 addresses, where a datagram
/// over the MTU is fragmented, which some receivers may never reassemble.
const _: () = assert!(IPV6_HEADER_LEN + UDP_HEADER_LEN + MAX_RETRANSMISSION_LEN <= ETHERNET_MTU);
pub(crate) const PAYLOAD_TYPE_AUDIO: u8 = 0x60;
const MARKER: u8 = 0x80;
const VERSION_2: u8 = 0x80;
const EXTENSION: u8 = 0x10;
const TYPE_TIMING_REQUEST: u8 = 0x52;
const TYPE_TIMING_REPLY: u8 = 0x53;
const TYPE_SYNC: u8 = 0x54;
const TYPE_RETRANSMIT_REQUEST: u8 = 0x55;
const TYPE_RETRANSMISSION: u8 = 0x56;
const SYNC_SEQ: u16 = 7;
const TIMING_SEQ: u16 = 7;

/// Claude: how an uncompressed ALAC frame says how many frames it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    /// Claude: it carries the count, as libraop writes every frame and as the
    /// AirPlay 1 receivers in use have been heard to play.
    Counted,
    /// Claude: it leaves the count out, so the decoder takes the stream's
    /// frames per packet, as Apple's encoder writes a full frame.
    Full,
}

pub(crate) const fn alac_frame_len(frames: usize, framing: Framing) -> usize {
    let count = match framing {
        Framing::Counted => ALAC_COUNT_BITS,
        Framing::Full => 0,
    };
    (ALAC_HEADER_BITS + count + 32 * frames + ALAC_END_BITS).div_ceil(8)
}

pub(crate) struct AudioHeader {
    pub(crate) seq: u16,
    pub(crate) timestamp: u32,
    pub(crate) ssrc: u32,
    pub(crate) marker: bool,
}

/// Claude: an RTP packet of payload type 96 carrying the frames as one
/// uncompressed ALAC frame of 16-bit stereo; returns its length.
pub(crate) fn write_audio_packet(
    out: &mut [u8; MAX_AUDIO_PACKET_LEN],
    header: &AudioHeader,
    left: &[f32; FRAMES_PER_PACKET],
    right: &[f32; FRAMES_PER_PACKET],
    framing: Framing,
) -> usize {
    out[0] = VERSION_2;
    out[1] = PAYLOAD_TYPE_AUDIO | if header.marker { MARKER } else { 0 };
    out[2..4].copy_from_slice(&header.seq.to_be_bytes());
    out[4..8].copy_from_slice(&header.timestamp.to_be_bytes());
    out[8..12].copy_from_slice(&header.ssrc.to_be_bytes());
    let frames = left
        .iter()
        .zip(right)
        .map(|(&l, &r)| [to_i16(l), to_i16(r)]);
    RTP_HEADER_LEN + write_alac_uncompressed(&mut out[RTP_HEADER_LEN..], frames, framing)
}

/// Claude: seals the payload of the `len`-byte packet in `out` as an AirPlay 2
/// receiver opens it, as pyatv sends it: ChaCha20-Poly1305 with the RTP
/// timestamp and SSRC as associated data, then the tag, then the nonce's
/// counter in little-endian. Returns the sealed packet's length.
pub(crate) fn seal_audio(key: &mut Counted, out: &mut [u8], len: usize) -> usize {
    let (header, payload) = out[..len].split_at_mut(RTP_HEADER_LEN);
    let (tag, counter) = key.seal(&header[4..12], payload);
    out[len..len + TAG_LEN].copy_from_slice(&tag);
    out[len + TAG_LEN..len + TAG_LEN + NONCE_LEN].copy_from_slice(&counter.to_le_bytes());
    len + TAG_LEN + NONCE_LEN
}

/// Claude: a packet of a buffered stream's data connection, as
/// shairport-sync reads one and Apple's sender was captured writing one: its
/// length in two big-endian bytes, counting themselves, then an RTP header
/// whose first four bytes are version 2 and a 23-bit sequence number, with no
/// payload type or marker, then the frames as one full ALAC frame sealed as a
/// realtime AirPlay 2 packet is. The SSRC names the audio format. Returns the
/// packet's length.
pub(crate) fn write_buffered_packet(
    out: &mut [u8; BUFFERED_PACKET_LEN],
    seq: u32,
    timestamp: u32,
    ssrc: u32,
    left: &[f32; FRAMES_PER_PACKET],
    right: &[f32; FRAMES_PER_PACKET],
    key: &mut Counted,
) -> usize {
    let (length, packet) = out.split_at_mut(BUFFERED_LENGTH_LEN);
    let first_word = (u32::from(VERSION_2) << 24) | (seq % BUFFERED_SEQUENCES);
    packet[..4].copy_from_slice(&first_word.to_be_bytes());
    packet[4..8].copy_from_slice(&timestamp.to_be_bytes());
    packet[8..12].copy_from_slice(&ssrc.to_be_bytes());
    let frames = left
        .iter()
        .zip(right)
        .map(|(&l, &r)| [to_i16(l), to_i16(r)]);
    let plain = RTP_HEADER_LEN
        + write_alac_uncompressed(&mut packet[RTP_HEADER_LEN..], frames, Framing::Full);
    let len = BUFFERED_LENGTH_LEN + seal_audio(key, packet, plain);
    length.copy_from_slice(&(len as u16).to_be_bytes());
    len
}

pub(crate) fn to_i16(sample: f32) -> i16 {
    (sample * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

/// Claude: a channel pair element flagged as not compressed and carrying its
/// frame count, the frames as big-endian 16-bit left and right, then the end
/// element, padded to a byte.
fn write_alac_uncompressed(
    out: &mut [u8],
    frames: impl ExactSizeIterator<Item = [i16; 2]>,
    framing: Framing,
) -> usize {
    let len = alac_frame_len(frames.len(), framing);
    out[..len].fill(0);
    let mut bits = BitWriter { out, bit: 0 };
    bits.put(1, 3);
    bits.put(0, 4);
    bits.put(0, 12);
    bits.put(u32::from(framing == Framing::Counted), 1);
    bits.put(0, 2);
    bits.put(1, 1);
    if framing == Framing::Counted {
        bits.put(frames.len() as u32, 32);
    }
    for [left, right] in frames {
        bits.put(u32::from(left as u16), 16);
        bits.put(u32::from(right as u16), 16);
    }
    bits.put(7, 3);
    len
}

struct BitWriter<'a> {
    out: &'a mut [u8],
    bit: usize,
}

impl BitWriter<'_> {
    fn put(&mut self, value: u32, bits: u32) {
        let mut remaining = bits;
        while remaining > 0 {
            let free = 8 - (self.bit % 8) as u32;
            let take = free.min(remaining);
            let chunk = (value >> (remaining - take)) & ((1 << take) - 1);
            self.out[self.bit / 8] |= (chunk << (free - take)) as u8;
            self.bit += take as usize;
            remaining -= take;
        }
    }
}

/// Claude: maps the RTP timeline to the stream's clock: the receiver plays
/// frame `playing`, plus whatever latency it adds itself, at `at`, while the
/// sender is sending frame `sending`.
pub(crate) struct Sync {
    pub(crate) first: bool,
    pub(crate) playing: u32,
    pub(crate) at: Ntp,
    pub(crate) sending: u32,
}

pub(crate) fn sync_packet(sync: &Sync) -> [u8; 20] {
    let mut out = [0; 20];
    out[0] = VERSION_2 | if sync.first { EXTENSION } else { 0 };
    out[1] = MARKER | TYPE_SYNC;
    out[2..4].copy_from_slice(&SYNC_SEQ.to_be_bytes());
    out[4..8].copy_from_slice(&sync.playing.to_be_bytes());
    out[8..16].copy_from_slice(&sync.at.to_be_bytes());
    out[16..20].copy_from_slice(&sync.sending.to_be_bytes());
    out
}

/// Claude: the time a timing request says it was sent, which the reply echoes
/// as its origin.
pub(crate) fn timing_request_sent_at(packet: &[u8]) -> Option<Ntp> {
    if packet.len() < 32 || packet[1] & !MARKER != TYPE_TIMING_REQUEST {
        return None;
    }
    Some(Ntp::from_be_bytes(packet[24..32].try_into().ok()?))
}

pub(crate) fn timing_reply(origin: Ntp, received: Ntp, sent: Ntp) -> [u8; 32] {
    let mut out = [0; 32];
    out[0] = VERSION_2;
    out[1] = MARKER | TYPE_TIMING_REPLY;
    out[2..4].copy_from_slice(&TIMING_SEQ.to_be_bytes());
    out[8..16].copy_from_slice(&origin.to_be_bytes());
    out[16..24].copy_from_slice(&received.to_be_bytes());
    out[24..32].copy_from_slice(&sent.to_be_bytes());
    out
}

/// Claude: the first sequence number and the count of the packets a
/// retransmit request asks for.
pub(crate) fn retransmit_request(packet: &[u8]) -> Option<(u16, u16)> {
    if packet.len() < 8 || packet[1] & !MARKER != TYPE_RETRANSMIT_REQUEST {
        return None;
    }
    Some((
        u16::from_be_bytes([packet[4], packet[5]]),
        u16::from_be_bytes([packet[6], packet[7]]),
    ))
}

pub(crate) fn write_retransmission(
    original: &[u8],
    out: &mut [u8; MAX_RETRANSMISSION_LEN],
) -> usize {
    out[0] = VERSION_2;
    out[1] = MARKER | TYPE_RETRANSMISSION;
    out[2..4].copy_from_slice(&original[2..4]);
    let len = RETRANSMISSION_HEADER_LEN + original.len();
    out[RETRANSMISSION_HEADER_LEN..len].copy_from_slice(original);
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(bit_string: &str) -> Vec<u8> {
        let bits: Vec<u8> = bit_string
            .bytes()
            .filter(|b| *b != b' ')
            .map(|b| b - b'0')
            .collect();
        bits.chunks(8)
            .map(|chunk| {
                chunk
                    .iter()
                    .enumerate()
                    .fold(0, |acc, (i, bit)| acc | bit << (7 - i))
            })
            .collect()
    }

    #[test]
    fn uncompressed_alac_frames_follow_the_bitstream_layout() {
        let mut out = [0xAA; 32];
        let frames = [[0x1234, 0xABCDu16 as i16], [-1, 0]];
        let len = write_alac_uncompressed(&mut out, frames.into_iter(), Framing::Counted);
        let expected = bytes(concat!(
            "001 0000 000000000000 1 00 1",
            "00000000000000000000000000000010",
            "0001001000110100 1010101111001101",
            "1111111111111111 0000000000000000",
            "111",
        ));
        assert_eq!(&out[..len], expected.as_slice());

        let mut out = [0xAA; 32];
        let len = write_alac_uncompressed(&mut out, frames.into_iter(), Framing::Full);
        let expected = bytes(concat!(
            "001 0000 000000000000 0 00 1",
            "0001001000110100 1010101111001101",
            "1111111111111111 0000000000000000",
            "111",
        ));
        assert_eq!(&out[..len], expected.as_slice());
    }

    #[test]
    fn a_full_packet_is_a_header_and_1416_bytes_of_alac() {
        assert_eq!(alac_frame_len(FRAMES_PER_PACKET, Framing::Counted), 1416);
        let mut out = [0; MAX_AUDIO_PACKET_LEN];
        let mut left = [0.0; FRAMES_PER_PACKET];
        let right = [0.0; FRAMES_PER_PACKET];
        left[0] = 0.5;
        let header = AudioHeader {
            seq: 0x1234,
            timestamp: 0x89AB_CDEF,
            ssrc: 0x0102_0304,
            marker: true,
        };
        assert_eq!(
            write_audio_packet(&mut out, &header, &left, &right, Framing::Counted),
            AIRPLAY1_PACKET_LEN
        );
        assert_eq!(
            out[..12],
            [0x80, 0xE0, 0x12, 0x34, 0x89, 0xAB, 0xCD, 0xEF, 1, 2, 3, 4]
        );
        assert_eq!(out[12..19], [0x20, 0x00, 0x12, 0x00, 0x00, 0x02, 0xC0]);
        assert_eq!(out[19], 0x80);
        assert_eq!(
            out[AIRPLAY1_PACKET_LEN - 2..AIRPLAY1_PACKET_LEN],
            [0x01, 0xC0]
        );

        let header = AudioHeader {
            marker: false,
            ..header
        };
        write_audio_packet(&mut out, &header, &left, &right, Framing::Counted);
        assert_eq!(out[1], 0x60);
    }

    /// Claude: the payload sealed with key 00..1f and counter 0, with the
    /// timestamp and SSRC bytes of the header as associated data, built with
    /// OpenSSL's ChaCha20 and Poly1305 as RFC 8439 composes them.
    const SEALED_TAG: &str = "bdc6bf7c0992d6aae3d35ceddd05a86b";

    #[test]
    fn an_airplay_2_packet_seals_its_full_alac_frame_and_fits_ipv6() {
        assert_eq!(alac_frame_len(FRAMES_PER_PACKET, Framing::Full), 1412);
        assert_eq!(AIRPLAY2_PACKET_LEN, 1448);
        assert_eq!(IPV6_HEADER_LEN + UDP_HEADER_LEN + AIRPLAY2_PACKET_LEN, 1496);
        assert_eq!(
            IPV6_HEADER_LEN + UDP_HEADER_LEN + RETRANSMISSION_HEADER_LEN + AIRPLAY1_PACKET_LEN,
            1480
        );
        let header = AudioHeader {
            seq: 0x1234,
            timestamp: 0x89AB_CDEF,
            ssrc: 0x0102_0304,
            marker: true,
        };
        let left: [f32; FRAMES_PER_PACKET] = std::array::from_fn(|i| i as f32 / 1024.0);
        let right: [f32; FRAMES_PER_PACKET] = std::array::from_fn(|i| -(i as f32) / 2048.0);
        let mut plain = [0; MAX_AUDIO_PACKET_LEN];
        let plain_len = write_audio_packet(&mut plain, &header, &left, &right, Framing::Full);
        assert_eq!(plain_len, 12 + 1412);
        assert_eq!(plain[12..15], [0x20, 0x00, 0x02]);

        let key = std::array::from_fn(|i| i as u8);
        let mut counted = Counted::new(&key);
        let mut sealed = plain;
        let len = seal_audio(&mut counted, &mut sealed, plain_len);
        assert_eq!(len, AIRPLAY2_PACKET_LEN);
        assert_eq!(sealed[..12], plain[..12]);
        assert!(sealed[12..plain_len] != plain[12..plain_len]);
        let tag_at = plain_len;
        assert_eq!(
            sealed[tag_at..tag_at + 16].to_vec(),
            crate::srp::tests::hex(SEALED_TAG)
        );
        assert_eq!(sealed[tag_at + 16..len], 0u64.to_le_bytes());
        let len = seal_audio(&mut counted, &mut plain.clone(), plain_len);
        assert_eq!(len, AIRPLAY2_PACKET_LEN);
        let mut again = plain;
        seal_audio(&mut counted, &mut again, plain_len);
        assert_eq!(again[len - 8..len], 2u64.to_le_bytes());
    }

    #[test]
    fn a_buffered_packet_is_its_length_a_23_bit_sequence_and_a_sealed_full_frame() {
        let left: [f32; FRAMES_PER_PACKET] = std::array::from_fn(|i| i as f32 / 1024.0);
        let right: [f32; FRAMES_PER_PACKET] = std::array::from_fn(|i| -(i as f32) / 2048.0);
        let key = std::array::from_fn(|i| i as u8);
        let header = AudioHeader {
            seq: 0,
            timestamp: 0x89AB_CDEF,
            ssrc: 0x0102_0304,
            marker: false,
        };
        let mut realtime = [0; MAX_AUDIO_PACKET_LEN];
        let plain = write_audio_packet(&mut realtime, &header, &left, &right, Framing::Full);
        seal_audio(&mut Counted::new(&key), &mut realtime, plain);

        let mut out = [0; BUFFERED_PACKET_LEN];
        let mut counted = Counted::new(&key);
        let len = write_buffered_packet(
            &mut out,
            0x0080_0000 + 0x19_8CEB,
            0x89AB_CDEF,
            0x0102_0304,
            &left,
            &right,
            &mut counted,
        );
        assert_eq!(len, BUFFERED_PACKET_LEN);
        assert_eq!(BUFFERED_PACKET_LEN, 1450);
        assert_eq!(out[..2], [0x05, 0xAA]);
        assert_eq!(
            out[2..14],
            [0x80, 0x19, 0x8C, 0xEB, 0x89, 0xAB, 0xCD, 0xEF, 1, 2, 3, 4]
        );
        assert_eq!(
            out[14..],
            realtime[12..AIRPLAY2_PACKET_LEN],
            "sealed as the realtime packet with the same timestamp, SSRC and counter"
        );
        let next = write_buffered_packet(
            &mut out,
            BUFFERED_SEQUENCES - 1,
            0,
            0,
            &left,
            &right,
            &mut counted,
        );
        assert_eq!(out[2..6], [0x80, 0x7F, 0xFF, 0xFF]);
        assert_eq!(out[next - 8..next], 1u64.to_le_bytes());
    }

    #[test]
    fn samples_scale_by_32768_and_saturate() {
        assert_eq!(to_i16(0.5), 16384);
        assert_eq!(to_i16(-1.0), -32768);
        assert_eq!(to_i16(1.0), 32767);
        assert_eq!(to_i16(3.0), 32767);
        assert_eq!(to_i16(f32::NAN), 0);
        for sample in [-32768i16, -1, 0, 1, 12345, 32767] {
            assert_eq!(to_i16(f32::from(sample) / 32768.0), sample);
        }
    }

    #[test]
    fn sync_packets_carry_both_rtp_times_around_the_ntp_time() {
        let sync = Sync {
            first: true,
            playing: 0x0102_0304,
            at: Ntp(0x1122_3344_5566_7788),
            sending: 0x0A0B_0C0D,
        };
        assert_eq!(
            sync_packet(&sync),
            [
                0x90, 0xD4, 0x00, 0x07, 0x01, 0x02, 0x03, 0x04, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
                0x77, 0x88, 0x0A, 0x0B, 0x0C, 0x0D
            ]
        );
        assert_eq!(
            sync_packet(&Sync {
                first: false,
                ..sync
            })[0],
            0x80
        );
    }

    #[test]
    fn timing_replies_echo_the_request_as_origin() {
        let mut request = [0u8; 32];
        request[..4].copy_from_slice(&[0x80, 0xD2, 0x00, 0x07]);
        request[24..32].copy_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
        let origin = timing_request_sent_at(&request).unwrap();
        assert_eq!(origin, Ntp(0x0102_0304_0506_0708));
        assert_eq!(timing_request_sent_at(&request[..31]), None);
        request[1] = 0xD4;
        assert_eq!(timing_request_sent_at(&request), None);

        let reply = timing_reply(origin, Ntp(0xA), Ntp(0xB));
        assert_eq!(reply[..8], [0x80, 0xD3, 0x00, 0x07, 0, 0, 0, 0]);
        assert_eq!(reply[8..16], 0x0102_0304_0506_0708u64.to_be_bytes());
        assert_eq!(reply[16..24], 0xAu64.to_be_bytes());
        assert_eq!(reply[24..32], 0xBu64.to_be_bytes());
    }

    #[test]
    fn retransmissions_wrap_the_original_packet() {
        assert_eq!(
            retransmit_request(&[0x80, 0xD5, 0x00, 0x01, 0xFF, 0xFE, 0x00, 0x03]),
            Some((0xFFFE, 3))
        );
        assert_eq!(
            retransmit_request(&[0x80, 0xD5, 0x00, 0x01, 0xFF, 0xFE, 0x00]),
            None
        );
        assert_eq!(
            retransmit_request(&[0x80, 0xD4, 0x00, 0x01, 0xFF, 0xFE, 0x00, 0x03]),
            None
        );

        let original: Vec<u8> = (0..AIRPLAY1_PACKET_LEN).map(|i| i as u8).collect();
        let mut out = [0; MAX_RETRANSMISSION_LEN];
        let len = write_retransmission(&original, &mut out);
        assert_eq!(len, 4 + AIRPLAY1_PACKET_LEN);
        assert_eq!(out[..4], [0x80, 0xD6, original[2], original[3]]);
        assert_eq!(&out[4..len], original.as_slice());
    }
}
