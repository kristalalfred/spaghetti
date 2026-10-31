pub(crate) const METHOD: u8 = 0x00;
pub(crate) const SALT: u8 = 0x02;
pub(crate) const PUBLIC_KEY: u8 = 0x03;
pub(crate) const PROOF: u8 = 0x04;
pub(crate) const STATE: u8 = 0x06;
pub(crate) const ERROR: u8 = 0x07;
pub(crate) const FLAGS: u8 = 0x13;

const MAX_ITEM: usize = 255;

/// Claude: HomeKit's TLV8 items, where a value longer than 255 bytes is split
/// across consecutive items of the same type.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Tlv(Vec<(u8, Vec<u8>)>);

impl Tlv {
    pub(crate) fn encode(items: &[(u8, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for &(kind, value) in items {
            if value.is_empty() {
                out.extend_from_slice(&[kind, 0]);
            }
            for chunk in value.chunks(MAX_ITEM) {
                out.extend_from_slice(&[kind, chunk.len() as u8]);
                out.extend_from_slice(chunk);
            }
        }
        out
    }

    pub(crate) fn decode(mut data: &[u8]) -> Result<Tlv, String> {
        let mut items: Vec<(u8, Vec<u8>)> = Vec::new();
        let mut continues = false;
        while let [kind, len, rest @ ..] = data {
            let len = usize::from(*len);
            let Some(value) = rest.get(..len) else {
                return Err(format!(
                    "a TLV8 item of type {kind:#04x} claims {len} bytes but {} remain",
                    rest.len()
                ));
            };
            match items.last_mut() {
                Some((last, joined)) if continues && last == kind => {
                    joined.extend_from_slice(value)
                }
                _ => items.push((*kind, value.to_vec())),
            }
            continues = len == MAX_ITEM;
            data = &rest[len..];
        }
        if !data.is_empty() {
            return Err("a TLV8 item is cut off after its type".into());
        }
        Ok(Tlv(items))
    }

    pub(crate) fn get(&self, kind: u8) -> Option<&[u8]> {
        self.0
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, v)| v.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_encode_as_pyatv_tests_them() {
        assert_eq!(Tlv::encode(&[(10, b"123")]), b"\x0a\x03\x31\x32\x33");
        assert_eq!(
            Tlv::encode(&[(1, b"111"), (4, b"222")]),
            b"\x01\x03\x31\x31\x31\x04\x03\x32\x32\x32"
        );
        let mut large = vec![0x02, 0xFF];
        large.extend_from_slice(&[0x31; 255]);
        large.extend_from_slice(&[0x02, 0x01, 0x31]);
        assert_eq!(Tlv::encode(&[(2, &[0x31; 256])]), large);
        assert_eq!(
            Tlv::encode(&[(6, b"\x01"), (0x13, b"")]),
            b"\x06\x01\x01\x13\x00"
        );
    }

    #[test]
    fn items_decode_joining_fragments_of_long_values() {
        let decoded = Tlv::decode(b"\x01\x03\x31\x31\x31\x04\x03\x32\x32\x32").unwrap();
        assert_eq!(decoded.get(1), Some(&b"111"[..]));
        assert_eq!(decoded.get(4), Some(&b"222"[..]));
        assert_eq!(decoded.get(2), None);

        let public = [0xAB; 384];
        let encoded = Tlv::encode(&[(PUBLIC_KEY, &public), (PROOF, &[0xCD; 64])]);
        let decoded = Tlv::decode(&encoded).unwrap();
        assert_eq!(decoded.get(PUBLIC_KEY), Some(&public[..]));
        assert_eq!(decoded.get(PROOF), Some(&[0xCD; 64][..]));

        let separate = Tlv::decode(b"\x01\x01\x41\x01\x01\x42").unwrap();
        assert_eq!(separate.get(1), Some(&b"A"[..]));
    }

    #[test]
    fn cut_off_items_are_refused() {
        assert!(Tlv::decode(b"\x01\x03\x31\x31").is_err());
        assert!(Tlv::decode(b"\x01\x01\x31\x02").is_err());
        assert_eq!(Tlv::decode(b""), Ok(Tlv::default()));
    }
}
