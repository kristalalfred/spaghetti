use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, Tag};
use hkdf::Hkdf;
use sha2::Sha512;

pub(crate) const KEY_LEN: usize = 32;
pub(crate) const TAG_LEN: usize = 16;
const LEN_LEN: usize = 2;
const MAX_FRAME: usize = 1024;

/// Claude: the keys one end of a connection seals and opens with.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Keys {
    pub(crate) write: [u8; KEY_LEN],
    pub(crate) read: [u8; KEY_LEN],
}

/// Claude: derives a connection's keys from the pairing's shared secret with
/// HKDF-SHA512, as pair_ap and pyatv do: `salt` with `write_info` for what
/// this end sends and `read_info` for what it receives.
pub(crate) fn keys(shared: &[u8], salt: &str, write_info: &str, read_info: &str) -> Keys {
    let derive = |info: &str| {
        let mut key = [0; KEY_LEN];
        Hkdf::<Sha512>::new(Some(salt.as_bytes()), shared)
            .expand(info.as_bytes(), &mut key)
            .expect("32 bytes is a valid HKDF-SHA512 length");
        key
    };
    Keys {
        write: derive(write_info),
        read: derive(read_info),
    }
}

/// Claude: ChaCha20-Poly1305 with the 96-bit nonce HomeKit builds from a
/// 64-bit counter: four zero bytes, then the counter in little-endian.
pub(crate) struct Counted {
    cipher: ChaCha20Poly1305,
    next: u64,
}

impl Counted {
    pub(crate) fn new(key: &[u8; KEY_LEN]) -> Counted {
        Counted {
            cipher: ChaCha20Poly1305::new(key.into()),
            next: 0,
        }
    }

    pub(crate) fn nonce(counter: u64) -> Nonce {
        let mut nonce = Nonce::default();
        nonce[4..].copy_from_slice(&counter.to_le_bytes());
        nonce
    }

    /// Claude: encrypts `data` in place and returns the tag and the counter
    /// it used.
    pub(crate) fn seal(&mut self, aad: &[u8], data: &mut [u8]) -> ([u8; TAG_LEN], u64) {
        let counter = self.next;
        self.next += 1;
        let tag = self
            .cipher
            .encrypt_in_place_detached(&Self::nonce(counter), aad, data)
            .expect("a frame is far below ChaCha20's limit");
        (tag.into(), counter)
    }

    fn open(&mut self, aad: &[u8], data: &mut [u8], tag: &[u8]) -> Result<(), ()> {
        let counter = self.next;
        self.next += 1;
        self.cipher
            .decrypt_in_place_detached(&Self::nonce(counter), aad, data, Tag::from_slice(tag))
            .map_err(|_| ())
    }
}

/// Claude: a connection encrypted as HomeKit's sessions are: each frame is a
/// two-byte little-endian length of at most 1024, the ciphertext of that many
/// bytes and its tag, with the length bytes as associated data and each
/// direction counting its own nonces from zero.
pub(crate) struct Channel {
    sealing: Counted,
    opening: Counted,
    pending: Vec<u8>,
}

impl Channel {
    pub(crate) fn new(keys: &Keys) -> Channel {
        Channel {
            sealing: Counted::new(&keys.write),
            opening: Counted::new(&keys.read),
            pending: Vec::new(),
        }
    }

    pub(crate) fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(plaintext.len() + plaintext.len().div_ceil(MAX_FRAME) * 18);
        for chunk in plaintext.chunks(MAX_FRAME) {
            let len = (chunk.len() as u16).to_le_bytes();
            out.extend_from_slice(&len);
            let start = out.len();
            out.extend_from_slice(chunk);
            let (tag, _) = self.sealing.seal(&len, &mut out[start..]);
            out.extend_from_slice(&tag);
        }
        out
    }

    /// Claude: takes bytes as they arrive and appends the plaintext of every
    /// frame they complete; a frame split across reads waits for the rest.
    pub(crate) fn open(&mut self, received: &[u8], plaintext: &mut Vec<u8>) -> Result<(), String> {
        self.pending.extend_from_slice(received);
        let mut at = 0;
        while let Some(len_bytes) = self.pending.get(at..at + LEN_LEN) {
            let len_bytes = [len_bytes[0], len_bytes[1]];
            let len = usize::from(u16::from_le_bytes(len_bytes));
            if len > MAX_FRAME {
                return Err(format!(
                    "an encrypted frame claims {len} bytes, over {MAX_FRAME}"
                ));
            }
            let end = at + LEN_LEN + len + TAG_LEN;
            if self.pending.len() < end {
                break;
            }
            let (body, tag) = self.pending[at + LEN_LEN..end].split_at_mut(len);
            self.opening
                .open(&len_bytes, body, tag)
                .map_err(|()| "an encrypted frame failed its authentication".to_owned())?;
            plaintext.extend_from_slice(body);
            at = end;
        }
        self.pending.drain(..at);
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::srp::tests::{hap, hex};

    /// Claude: the control and event keys derived from the HomeKit vectors'
    /// session key K, computed with `openssl kdf ... HKDF`.
    const CONTROL_WRITE: &str = "e4ae7f784030893fcd0345eaf0b433651afe78fe541591a1ecfa1a5d4caf196c";
    const CONTROL_READ: &str = "658e314382c90c4c2ea97303aaff9e41e570647f42ca80461e1e6bd1e3af3ea6";
    const EVENTS_WRITE: &str = "db3532c3b1378bb2c5272ca1b74e1de51e824befa77ef54ae247255ea3d03395";
    const EVENTS_READ: &str = "48fa62bcbaa37487d03b6ab23387d8fa6e5e18b0e7680e795647482ba22abc16";

    #[test]
    fn keys_derive_from_the_shared_secret_with_hkdf_sha512() {
        let key = hex(hap::KEY);
        let control = keys(
            &key,
            "Control-Salt",
            "Control-Write-Encryption-Key",
            "Control-Read-Encryption-Key",
        );
        assert_eq!(control.write.to_vec(), hex(CONTROL_WRITE));
        assert_eq!(control.read.to_vec(), hex(CONTROL_READ));
        let events = keys(
            &key,
            "Events-Salt",
            "Events-Read-Encryption-Key",
            "Events-Write-Encryption-Key",
        );
        assert_eq!(events.write.to_vec(), hex(EVENTS_READ));
        assert_eq!(events.read.to_vec(), hex(EVENTS_WRITE));
    }

    pub(crate) fn test_keys() -> Keys {
        Keys {
            write: std::array::from_fn(|i| i as u8),
            read: std::array::from_fn(|i| 0x80 + i as u8),
        }
    }

    pub(crate) fn reversed(keys: &Keys) -> Keys {
        Keys {
            write: keys.read,
            read: keys.write,
        }
    }

    /// Claude: two frames sealed with key 00..1f, the first of 1024 bytes
    /// counting 0, 1, 2, ... and the second "tail" with counter 1, built with
    /// OpenSSL's ChaCha20 and Poly1305 as RFC 8439 composes them.
    const FIRST_FRAME_TAG: &str = "75bb53d11deffd60b0291b1b30bdc0d8";
    const SECOND_FRAME: &str = "0400eb369a3379b4e015f4ab364d8d73b4eb093c6bd0";

    fn message() -> Vec<u8> {
        let mut message: Vec<u8> = (0..1024).map(|i| i as u8).collect();
        message.extend_from_slice(b"tail");
        message
    }

    #[test]
    fn frames_carry_at_most_1024_bytes_with_counted_nonces() {
        let sealed = Channel::new(&test_keys()).seal(&message());
        assert_eq!(sealed.len(), 2 + 1024 + 16 + 2 + 4 + 16);
        assert_eq!(sealed[..2], [0x00, 0x04]);
        assert_eq!(
            sealed[2 + 1024..2 + 1024 + 16].to_vec(),
            hex(FIRST_FRAME_TAG)
        );
        assert_eq!(sealed[2 + 1024 + 16..].to_vec(), hex(SECOND_FRAME));
    }

    #[test]
    fn frames_open_however_they_arrive() {
        let keys = test_keys();
        let mut sender = Channel::new(&reversed(&keys));
        let mut wire = sender.seal(&message());
        wire.extend_from_slice(&sender.seal(b"next"));
        for split in [1, 2, 3, 1042, 1043, 1044, wire.len() - 1, wire.len()] {
            let mut receiver = Channel::new(&keys);
            let mut plaintext = Vec::new();
            receiver.open(&wire[..split], &mut plaintext).unwrap();
            receiver.open(&wire[split..], &mut plaintext).unwrap();
            assert_eq!(
                plaintext,
                [message(), b"next".to_vec()].concat(),
                "split at {split}"
            );
        }
        let mut receiver = Channel::new(&keys);
        let mut plaintext = Vec::new();
        for byte in &wire {
            receiver.open(&[*byte], &mut plaintext).unwrap();
        }
        assert_eq!(plaintext, [message(), b"next".to_vec()].concat());
    }

    #[test]
    fn tampered_reordered_or_oversized_frames_are_refused() {
        let keys = test_keys();
        let mut sender = Channel::new(&reversed(&keys));
        let first = sender.seal(b"first");
        let second = sender.seal(b"second");
        let opened = |wire: &[u8]| Channel::new(&keys).open(wire, &mut Vec::new());

        let mut tampered = first.clone();
        tampered[3] ^= 1;
        assert!(opened(&tampered).is_err());
        let mut tag = first.clone();
        let last = tag.len() - 1;
        tag[last] ^= 1;
        assert!(opened(&tag).is_err());
        let mut length = first.clone();
        length[0] = 4;
        assert!(opened(&length).is_err());
        assert!(
            opened(&second).is_err(),
            "a frame opened with the wrong counter"
        );
        assert!(opened(&[0x01, 0x04]).unwrap_err().contains("1025 bytes"));
        assert!(opened(&[&first[..], &second[..]].concat()).is_ok());
    }

    #[test]
    fn each_session_derives_its_own_keys() {
        let first = keys(
            &[1; 64],
            "Control-Salt",
            "Control-Write-Encryption-Key",
            "Control-Read-Encryption-Key",
        );
        let second = keys(
            &[2; 64],
            "Control-Salt",
            "Control-Write-Encryption-Key",
            "Control-Read-Encryption-Key",
        );
        assert!(first.write != second.write && first.read != second.read);
        assert!(first.write != first.read);
        let sealed = Channel::new(&first).seal(b"same");
        assert!(sealed != Channel::new(&second).seal(b"same"));
        assert!(
            Channel::new(&reversed(&second))
                .open(&sealed, &mut Vec::new())
                .is_err()
        );
    }
}
