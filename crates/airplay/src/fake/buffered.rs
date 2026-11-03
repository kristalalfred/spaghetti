use std::io::{ErrorKind, Read};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, Tag};
use socket2::{Domain, Socket, Type};

use super::{POLL, State, decode_alac};

const LENGTH_LEN: usize = 2;
const HEADER_LEN: usize = 12;
const TAG_LEN: usize = 16;
const NONCE_LEN: usize = 8;
/// Claude: shairport-sync takes a packet only while what follows its length
/// is under 16 KiB.
const MAX_AFTER_LENGTH: usize = 16 * 1024;
/// Claude: the receive buffer the fake asks for, so that a sender facing a
/// reader that stops fills it within a fraction of a second.
const RECEIVE_BUFFER: usize = 16 * 1024;

/// Claude: how the fake reads a buffered stream's data connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Reading {
    #[default]
    Steady,
    ByteAtATime,
    /// Claude: stops for `pause` once `after` bytes have come, then reads on.
    Pausing {
        after: usize,
        pause: Duration,
    },
    /// Claude: accepts the connection and never reads from it.
    Never,
    /// Claude: closes the connection once `after` bytes have come.
    ClosingAfter(usize),
}

/// Claude: one packet taken from a buffered stream's data connection, before
/// it is opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Framed {
    pub(crate) version: u8,
    pub(crate) marker: bool,
    pub(crate) seq: u32,
    pub(crate) timestamp: u32,
    pub(crate) ssrc: u32,
    pub(crate) nonce: u64,
    aad: [u8; 8],
    ciphertext: Vec<u8>,
    tag: [u8; TAG_LEN],
}

/// Claude: a buffered packet opened and its uncompressed ALAC frame read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BufferedPacket {
    pub(crate) seq: u32,
    pub(crate) timestamp: u32,
    pub(crate) ssrc: u32,
    pub(crate) nonce: u64,
    pub(crate) frames: Vec<[i16; 2]>,
}

/// Claude: reads a buffered stream's data connection as shairport-sync's
/// buffered audio processor describes it, written from that description and
/// not from the sender's code: each packet is a big-endian length counting
/// its own two bytes, then a header whose first four bytes hold the sequence
/// number in their low 23 bits, the RTP time and the SSRC, then the
/// ChaCha20-Poly1305 ciphertext, its tag and the last eight bytes of its
/// nonce. Bytes come in however the connection splits or joins them.
#[derive(Default)]
pub(crate) struct DataReader {
    held: Vec<u8>,
}

impl DataReader {
    /// Claude: the packets `bytes` complete; a length no packet can have
    /// fails the test.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<Framed> {
        self.held.extend_from_slice(bytes);
        let mut framed = Vec::new();
        let mut at = 0;
        while self.held.len() - at >= LENGTH_LEN {
            let length = usize::from(u16::from_be_bytes([self.held[at], self.held[at + 1]]));
            assert!(
                length >= LENGTH_LEN + HEADER_LEN + TAG_LEN + NONCE_LEN
                    && length - LENGTH_LEN < MAX_AFTER_LENGTH,
                "a packet {length} bytes long"
            );
            if self.held.len() - at < length {
                break;
            }
            framed.push(frame(&self.held[at + LENGTH_LEN..at + length]));
            at += length;
        }
        self.held.drain(..at);
        framed
    }

    /// Claude: how many bytes of an unfinished packet it holds.
    pub(crate) fn held(&self) -> usize {
        self.held.len()
    }
}

fn frame(packet: &[u8]) -> Framed {
    let word = |at: usize| u32::from_be_bytes(packet[at..at + 4].try_into().unwrap());
    let (sealed, nonce) = packet.split_at(packet.len() - NONCE_LEN);
    let (ciphertext, tag) = sealed[HEADER_LEN..].split_at(sealed.len() - HEADER_LEN - TAG_LEN);
    Framed {
        version: packet[0] >> 6,
        marker: word(0) & 0x0080_0000 != 0,
        seq: word(0) & 0x007F_FFFF,
        timestamp: word(4),
        ssrc: word(8),
        nonce: u64::from_le_bytes(nonce.try_into().unwrap()),
        aad: packet[4..12].try_into().unwrap(),
        ciphertext: ciphertext.to_vec(),
        tag: tag.try_into().unwrap(),
    }
}

impl Framed {
    /// Claude: opens the packet with the stream's key, the nonce four zero
    /// bytes and then the packet's eight, and reads its ALAC frame; a packet
    /// that does not open fails the test.
    pub(crate) fn open(&self, key: &[u8; 32]) -> BufferedPacket {
        let mut nonce = Nonce::default();
        nonce[4..].copy_from_slice(&self.nonce.to_le_bytes());
        let mut payload = self.ciphertext.clone();
        ChaCha20Poly1305::new(key.into())
            .decrypt_in_place_detached(&nonce, &self.aad, &mut payload, Tag::from_slice(&self.tag))
            .expect("the packet opens with the stream's key");
        let (counted, frames) = decode_alac(&payload);
        assert!(!counted, "a buffered ALAC frame is full");
        BufferedPacket {
            seq: self.seq,
            timestamp: self.timestamp,
            ssrc: self.ssrc,
            nonce: self.nonce,
            frames,
        }
    }
}

/// Claude: opens the port a buffered stream's data connection comes to and
/// reads the one connection it takes as `reading` says, keeping each packet
/// opened with `key` and when it came, and when the connection ended.
pub(super) fn listen_for_data(state: Arc<State>, key: [u8; 32], reading: Reading) -> u16 {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    socket.set_recv_buffer_size(RECEIVE_BUFFER).unwrap();
    socket
        .bind(&SocketAddr::from((Ipv4Addr::LOCALHOST, 0)).into())
        .unwrap();
    socket.listen(1).unwrap();
    let listener: TcpListener = socket.into();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(_) if state.stop.load(Ordering::Acquire) => return,
                Err(_) => thread::sleep(POLL),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let mut reader = DataReader::default();
        let mut taken = 0;
        let mut paused = false;
        let mut chunk = [0u8; 4096];
        let end = loop {
            if state.stop.load(Ordering::Acquire) {
                break None;
            }
            let limit = match reading {
                Reading::Never => {
                    thread::sleep(POLL);
                    continue;
                }
                Reading::ByteAtATime => 1,
                Reading::Pausing { after, pause } if !paused && taken >= after => {
                    paused = true;
                    thread::sleep(pause);
                    continue;
                }
                Reading::Pausing { after, .. } if !paused => (after - taken).min(chunk.len()),
                Reading::ClosingAfter(after) if taken >= after => {
                    let _ = stream.shutdown(Shutdown::Both);
                    break Some(Instant::now());
                }
                Reading::ClosingAfter(after) => (after - taken).min(chunk.len()),
                _ => chunk.len(),
            };
            match stream.read(&mut chunk[..limit]) {
                Ok(0) => break Some(Instant::now()),
                Ok(n) => {
                    let at = Instant::now();
                    taken += n;
                    let framed = reader.feed(&chunk[..n]);
                    let mut data = state.data.lock().unwrap();
                    data.packets
                        .extend(framed.iter().map(|framed| (at, framed.open(&key))));
                    data.bytes = taken;
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(_) => break Some(Instant::now()),
            }
        };
        state.data.lock().unwrap().ended = end;
    });
    port
}

#[cfg(test)]
mod streams;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hap::Counted;
    use crate::packet::{BUFFERED_PACKET_LEN, BUFFERED_SEQUENCES, write_buffered_packet};
    use crate::{FRAMES_PER_PACKET, packet};

    /// Claude: the first two TCP segments of the data connection in a capture
    /// of Apple Music on macOS streaming silence to an HK Citation MultiBeam
    /// 1100: 1428 bytes each, so packets straddle them.
    const APPLE_SEGMENTS: &[u8] = include_bytes!("apple-buffered-segments.bin");

    #[test]
    fn apples_captured_packets_frame_with_their_length_and_23_bit_sequence() {
        let mut reader = DataReader::default();
        let (first, second) = APPLE_SEGMENTS.split_at(1428);
        let mut framed = reader.feed(first);
        assert_eq!((framed.len(), reader.held()), (32, 20));
        framed.extend(reader.feed(second));
        assert_eq!((framed.len(), reader.held()), (64, 40));
        for (i, packet) in framed.iter().enumerate() {
            let i32 = i as u32;
            assert_eq!(packet.version, 2);
            assert!(!packet.marker);
            assert_eq!(packet.seq, 0x19_8CEB + i32);
            assert_eq!(packet.timestamp, 0xA6BE_B911 + 1024 * i32);
            assert_eq!(packet.ssrc, 0x1600_0000, "AAC-LC 44.1 kHz");
            assert_eq!(packet.nonce, i as u64);
            assert_eq!(packet.ciphertext.len(), 44 - 2 - 12 - 16 - 8);
        }
    }

    fn frames_for(n: u32) -> ([f32; FRAMES_PER_PACKET], [f32; FRAMES_PER_PACKET]) {
        let sample = |i: usize, channel: u32| {
            f32::from(((n * 7 + i as u32 * 13 + channel) % 60_000) as u16 as i16) / 32768.0
        };
        (
            std::array::from_fn(|i| sample(i, 0)),
            std::array::from_fn(|i| sample(i, 1)),
        )
    }

    /// Claude: the sender's packets, as a run across the sequence wrap.
    fn sent(count: u32, first_seq: u32, key: &[u8; 32]) -> Vec<u8> {
        let mut counted = Counted::new(key);
        let mut bytes = Vec::new();
        for n in 0..count {
            let (left, right) = frames_for(n);
            let mut out = [0; BUFFERED_PACKET_LEN];
            let len = write_buffered_packet(
                &mut out,
                first_seq.wrapping_add(n),
                0xFFFF_FE00u32.wrapping_add(n * FRAMES_PER_PACKET as u32),
                0x1200_0000,
                &left,
                &right,
                &mut counted,
            );
            bytes.extend_from_slice(&out[..len]);
        }
        bytes
    }

    #[test]
    fn the_senders_packets_open_and_decode_however_the_connection_splits_them() {
        let key = [0x5A; 32];
        let first_seq = BUFFERED_SEQUENCES - 3;
        let bytes = sent(8, first_seq, &key);
        for chunks in [
            vec![bytes.len()],
            vec![1],
            vec![2, 3, 1449, 1451, 7],
            vec![4000],
        ] {
            let mut reader = DataReader::default();
            let mut packets = Vec::new();
            let mut rest = &bytes[..];
            for size in chunks.iter().cycle() {
                if rest.is_empty() {
                    break;
                }
                let (now, later) = rest.split_at((*size).min(rest.len()));
                packets.extend(reader.feed(now).iter().map(|f| f.open(&key)));
                rest = later;
            }
            assert_eq!((packets.len(), reader.held()), (8, 0), "{chunks:?}");
            for (n, packet) in packets.iter().enumerate() {
                let n32 = n as u32;
                assert_eq!(packet.seq, (first_seq + n32) % BUFFERED_SEQUENCES);
                assert_eq!(
                    packet.timestamp,
                    0xFFFF_FE00u32.wrapping_add(n32 * FRAMES_PER_PACKET as u32)
                );
                assert_eq!(packet.ssrc, 0x1200_0000);
                assert_eq!(packet.nonce, n as u64);
                let (left, right) = frames_for(n32);
                let expected: Vec<[i16; 2]> = left
                    .iter()
                    .zip(&right)
                    .map(|(&l, &r)| [packet::to_i16(l), packet::to_i16(r)])
                    .collect();
                assert_eq!(packet.frames, expected);
            }
            assert_eq!(packets[3].seq, 0, "the sequence wraps at 23 bits");
        }
    }

    #[test]
    #[should_panic(expected = "the packet opens with the stream's key")]
    fn a_packet_sealed_with_another_key_does_not_open() {
        let bytes = sent(1, 0, &[1; 32]);
        DataReader::default().feed(&bytes)[0].open(&[2; 32]);
    }

    mod listening {
        use std::io::Write;
        use std::net::TcpStream;

        use super::*;
        use crate::fake::{Behavior, Fake, wait_until};

        const KEY: [u8; 32] = [0x77; 32];

        /// Claude: a fake listening for a data connection read as `reading`,
        /// and a connection to it.
        fn listening(reading: Reading) -> (Fake, TcpStream) {
            let fake = Fake::start(Behavior::default());
            let port = listen_for_data(fake.state.clone(), KEY, reading);
            let connection = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            (fake, connection)
        }

        #[test]
        fn the_fake_reads_its_data_connection_as_told() {
            let bytes = sent(6, 0, &KEY);
            let whole = bytes.len();
            let (fake, mut connection) = listening(Reading::ByteAtATime);
            connection.write_all(&bytes).unwrap();
            assert!(wait_until(Duration::from_secs(5), || fake
                .buffered_audio()
                .len()
                == 6));
            assert_eq!(fake.data_bytes(), whole);

            let (fake, mut connection) = listening(Reading::Never);
            connection.write_all(&bytes).unwrap();
            thread::sleep(Duration::from_millis(100));
            assert_eq!(fake.data_bytes(), 0);

            let pause = Duration::from_millis(300);
            let (fake, mut connection) = listening(Reading::Pausing { after: 2000, pause });
            connection.write_all(&bytes).unwrap();
            assert!(wait_until(Duration::from_secs(1), || fake.data_bytes() == 2000));
            thread::sleep(pause / 2);
            assert_eq!(fake.data_bytes(), 2000, "paused");
            assert!(wait_until(Duration::from_secs(1), || fake.data_bytes() == whole));

            let (fake, mut connection) = listening(Reading::ClosingAfter(2000));
            connection.write_all(&bytes).unwrap();
            assert!(wait_until(Duration::from_secs(1), || fake
                .data_ended()
                .is_some()));
            assert_eq!((fake.data_bytes(), fake.buffered_audio().len()), (2000, 1));
        }
    }
}
