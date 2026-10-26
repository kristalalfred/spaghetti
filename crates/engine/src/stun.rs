use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use sha1::Sha1;

const HEADER_LEN: usize = 20;
const MAGIC_COOKIE: u32 = 0x2112_A442;
const IPV4: u8 = 0x01;
const INTEGRITY_LEN: usize = 20;

pub type Transaction = [u8; 12];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Binding,
    Allocate,
    Refresh,
    Send,
    Data,
    CreatePermission,
    ChannelBind,
}

impl Method {
    const ALL: [Self; 7] = [
        Self::Binding,
        Self::Allocate,
        Self::Refresh,
        Self::Send,
        Self::Data,
        Self::CreatePermission,
        Self::ChannelBind,
    ];

    fn code(self) -> u16 {
        match self {
            Self::Binding => 0x001,
            Self::Allocate => 0x003,
            Self::Refresh => 0x004,
            Self::Send => 0x006,
            Self::Data => 0x007,
            Self::CreatePermission => 0x008,
            Self::ChannelBind => 0x009,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Request,
    Indication,
    Success,
    Error,
}

impl Class {
    const ALL: [Self; 4] = [Self::Request, Self::Indication, Self::Success, Self::Error];

    fn bits(self) -> u16 {
        match self {
            Self::Request => 0b00,
            Self::Indication => 0b01,
            Self::Success => 0b10,
            Self::Error => 0b11,
        }
    }
}

/// Claude: RFC 5389 interleaves the class bits into the method bits.
fn message_type(method: Method, class: Class) -> u16 {
    let (method, class) = (method.code(), class.bits());
    (method & 0x000F)
        | ((method & 0x0070) << 1)
        | ((method & 0x0F80) << 2)
        | ((class & 0b01) << 4)
        | ((class & 0b10) << 7)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Attr {
    MappedAddress = 0x0001,
    Username = 0x0006,
    MessageIntegrity = 0x0008,
    ErrorCode = 0x0009,
    Lifetime = 0x000D,
    XorPeerAddress = 0x0012,
    Data = 0x0013,
    Realm = 0x0014,
    Nonce = 0x0015,
    XorRelayedAddress = 0x0016,
    RequestedTransport = 0x0019,
    XorMappedAddress = 0x0020,
}

impl Attr {
    fn is_xor(self) -> bool {
        matches!(
            self,
            Self::XorPeerAddress | Self::XorRelayedAddress | Self::XorMappedAddress
        )
    }
}

pub fn transaction_id() -> Transaction {
    let mut transaction = [0; 12];
    getrandom::fill(&mut transaction).expect("system randomness");
    transaction
}

pub fn binding_request(transaction: Transaction) -> Vec<u8> {
    MessageBuilder::new(Method::Binding, Class::Request, transaction).finish()
}

/// Claude: the transaction of anything shaped like a STUN message.
pub fn transaction(packet: &[u8]) -> Option<Transaction> {
    let header = packet.first_chunk::<HEADER_LEN>()?;
    if header[0] & 0xC0 != 0 || header[4..8] != MAGIC_COOKIE.to_be_bytes() {
        return None;
    }
    header[8..].try_into().ok()
}

/// Claude: the address a binding success response to `transaction` reports.
pub fn mapped_address(response: &[u8], transaction: Transaction) -> Option<SocketAddrV4> {
    let message = Message::parse(response)?;
    if (message.method, message.class) != (Method::Binding, Class::Success)
        || message.transaction != transaction
    {
        return None;
    }
    message
        .address(Attr::XorMappedAddress)
        .or_else(|| message.address(Attr::MappedAddress))
}

/// Claude: RFC 5389 section 15.4's key, without SASLprep.
pub fn long_term_key(username: &str, realm: &str, password: &str) -> [u8; 16] {
    Md5::digest(format!("{username}:{realm}:{password}")).into()
}

fn hmac_sha1(key: &[u8], message: &[u8]) -> Hmac<Sha1> {
    let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(message);
    mac
}

fn set_length(message: &mut [u8], length: usize) {
    let length = u16::try_from(length).expect("a STUN message fits in 64 KiB");
    message[2..4].copy_from_slice(&length.to_be_bytes());
}

pub struct MessageBuilder {
    bytes: Vec<u8>,
}

impl MessageBuilder {
    pub fn new(method: Method, class: Class, transaction: Transaction) -> Self {
        let mut bytes = Vec::with_capacity(HEADER_LEN + 64);
        bytes.extend_from_slice(&message_type(method, class).to_be_bytes());
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        bytes.extend_from_slice(&transaction);
        Self { bytes }
    }

    pub fn attribute(mut self, kind: Attr, value: &[u8]) -> Self {
        let length = u16::try_from(value.len()).expect("a STUN attribute fits in 64 KiB");
        self.bytes.extend_from_slice(&(kind as u16).to_be_bytes());
        self.bytes.extend_from_slice(&length.to_be_bytes());
        self.bytes.extend_from_slice(value);
        self.bytes.resize(self.bytes.len().next_multiple_of(4), 0);
        self
    }

    pub fn address(self, kind: Attr, address: SocketAddrV4) -> Self {
        let address = if kind.is_xor() { xor(address) } else { address };
        let [port_high, port_low] = address.port().to_be_bytes();
        let [a, b, c, d] = address.ip().octets();
        self.attribute(kind, &[0, IPV4, port_high, port_low, a, b, c, d])
    }

    pub fn finish(mut self) -> Vec<u8> {
        let length = self.bytes.len() - HEADER_LEN;
        set_length(&mut self.bytes, length);
        self.bytes
    }

    /// Claude: the HMAC covers a header whose length already counts it.
    pub fn integrity(mut self, key: &[u8]) -> Vec<u8> {
        let length = self.bytes.len() - HEADER_LEN + 4 + INTEGRITY_LEN;
        set_length(&mut self.bytes, length);
        let mac = hmac_sha1(key, &self.bytes).finalize().into_bytes();
        self.attribute(Attr::MessageIntegrity, &mac).bytes
    }
}

pub struct Message<'a> {
    pub method: Method,
    pub class: Class,
    pub transaction: Transaction,
    packet: &'a [u8],
}

struct Attribute<'a> {
    kind: u16,
    value: &'a [u8],
    offset: usize,
}

impl<'a> Message<'a> {
    pub fn parse(packet: &'a [u8]) -> Option<Self> {
        let (header, body) = packet.split_first_chunk::<HEADER_LEN>()?;
        let kind = u16::from_be_bytes([header[0], header[1]]);
        if kind & 0xC000 != 0
            || header[4..8] != MAGIC_COOKIE.to_be_bytes()
            || usize::from(u16::from_be_bytes([header[2], header[3]])) != body.len()
            || body.len() % 4 != 0
        {
            return None;
        }
        let method = Method::ALL
            .into_iter()
            .find(|method| message_type(*method, Class::Request) == kind & !0x0110)?;
        let class = Class::ALL
            .into_iter()
            .find(|class| message_type(method, *class) == kind)?;
        Some(Self {
            method,
            class,
            transaction: header[8..].try_into().ok()?,
            packet,
        })
    }

    fn attributes(&self) -> impl Iterator<Item = Attribute<'a>> + use<'a> {
        let packet = self.packet;
        let mut offset = HEADER_LEN;
        std::iter::from_fn(move || {
            let head = packet.get(offset..offset + 4)?;
            let length = usize::from(u16::from_be_bytes([head[2], head[3]]));
            let attribute = Attribute {
                kind: u16::from_be_bytes([head[0], head[1]]),
                value: packet.get(offset + 4..offset + 4 + length)?,
                offset,
            };
            offset += 4 + length.next_multiple_of(4);
            Some(attribute)
        })
    }

    pub fn attribute(&self, kind: Attr) -> Option<&'a [u8]> {
        self.attributes()
            .find(|attribute| attribute.kind == kind as u16)
            .map(|attribute| attribute.value)
    }

    pub fn address(&self, kind: Attr) -> Option<SocketAddrV4> {
        let &[_, IPV4, port_high, port_low, a, b, c, d] = self.attribute(kind)? else {
            return None;
        };
        let address = SocketAddrV4::new(
            Ipv4Addr::new(a, b, c, d),
            u16::from_be_bytes([port_high, port_low]),
        );
        Some(if kind.is_xor() { xor(address) } else { address })
    }

    pub fn error_code(&self) -> Option<u16> {
        let &[_, _, class, number, ..] = self.attribute(Attr::ErrorCode)? else {
            return None;
        };
        Some(u16::from(class & 0x07) * 100 + u16::from(number))
    }

    pub fn lifetime(&self) -> Option<Duration> {
        let seconds: [u8; 4] = self.attribute(Attr::Lifetime)?.try_into().ok()?;
        Some(Duration::from_secs(u32::from_be_bytes(seconds).into()))
    }

    pub fn integrity(&self, key: &[u8]) -> Option<bool> {
        let attribute = self
            .attributes()
            .find(|attribute| attribute.kind == Attr::MessageIntegrity as u16)?;
        let mut signed = self.packet[..attribute.offset].to_vec();
        set_length(
            &mut signed,
            attribute.offset - HEADER_LEN + 4 + INTEGRITY_LEN,
        );
        Some(hmac_sha1(key, &signed).verify_slice(attribute.value).is_ok())
    }
}

/// Claude: the XOR kinds differ from the plain one only by this mask, so it
/// both encodes and decodes.
fn xor(address: SocketAddrV4) -> SocketAddrV4 {
    let [cookie_high, cookie_low, ..] = MAGIC_COOKIE.to_be_bytes();
    SocketAddrV4::new(
        Ipv4Addr::from_bits(address.ip().to_bits() ^ MAGIC_COOKIE),
        address.port() ^ u16::from_be_bytes([cookie_high, cookie_low]),
    )
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Claude: a binding success response to `transaction` that maps to `mapped`.
    pub fn binding_response(transaction: Transaction, mapped: SocketAddrV4) -> Vec<u8> {
        MessageBuilder::new(Method::Binding, Class::Success, transaction)
            .address(Attr::XorMappedAddress, mapped)
            .finish()
    }

    pub fn error_code(code: u16) -> [u8; 4] {
        [0, 0, (code / 100) as u8, (code % 100) as u8]
    }

    /// Claude: the sample IPv4 response of RFC 5769, section 2.2.
    const RFC_5769_RESPONSE: [u8; 80] = [
        0x01, 0x01, 0x00, 0x3c, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6,
        0x86, 0xfa, 0x87, 0xdf, 0xae, 0x80, 0x22, 0x00, 0x0b, 0x74, 0x65, 0x73, 0x74, 0x20, 0x76,
        0x65, 0x63, 0x74, 0x6f, 0x72, 0x20, 0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1,
        0x12, 0xa6, 0x43, 0x00, 0x08, 0x00, 0x14, 0x2b, 0x91, 0xf5, 0x99, 0xfd, 0x9e, 0x90, 0xc3,
        0x8c, 0x74, 0x89, 0xf9, 0x2a, 0xf9, 0xba, 0x53, 0xf0, 0x6b, 0xe7, 0xd7, 0x80, 0x28, 0x00,
        0x04, 0xc0, 0x7d, 0x4c, 0x96,
    ];
    const RFC_5769_TRANSACTION: Transaction = [
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
    ];

    /// Claude: the long-term authentication sample of RFC 5769, section 2.4.
    const RFC_5769_LONG_TERM_REQUEST: [u8; 116] = [
        0x00, 0x01, 0x00, 0x60, 0x21, 0x12, 0xa4, 0x42, 0x78, 0xad, 0x34, 0x33, 0xc6, 0xad, 0x72,
        0xc0, 0x29, 0xda, 0x41, 0x2e, 0x00, 0x06, 0x00, 0x12, 0xe3, 0x83, 0x9e, 0xe3, 0x83, 0x88,
        0xe3, 0x83, 0xaa, 0xe3, 0x83, 0x83, 0xe3, 0x82, 0xaf, 0xe3, 0x82, 0xb9, 0x00, 0x00, 0x00,
        0x15, 0x00, 0x1c, 0x66, 0x2f, 0x2f, 0x34, 0x39, 0x39, 0x6b, 0x39, 0x35, 0x34, 0x64, 0x36,
        0x4f, 0x4c, 0x33, 0x34, 0x6f, 0x4c, 0x39, 0x46, 0x53, 0x54, 0x76, 0x79, 0x36, 0x34, 0x73,
        0x41, 0x00, 0x14, 0x00, 0x0b, 0x65, 0x78, 0x61, 0x6d, 0x70, 0x6c, 0x65, 0x2e, 0x6f, 0x72,
        0x67, 0x00, 0x00, 0x08, 0x00, 0x14, 0xf6, 0x70, 0x24, 0x65, 0x6d, 0xd6, 0x4a, 0x3e, 0x02,
        0xb8, 0xe0, 0x71, 0x2e, 0x85, 0xc9, 0xa2, 0x8c, 0xa8, 0x96, 0x66,
    ];
    const RFC_5769_LONG_TERM_TRANSACTION: Transaction = [
        0x78, 0xad, 0x34, 0x33, 0xc6, 0xad, 0x72, 0xc0, 0x29, 0xda, 0x41, 0x2e,
    ];
    const RFC_5769_USERNAME: &str = "\u{30DE}\u{30C8}\u{30EA}\u{30C3}\u{30AF}\u{30B9}";

    #[test]
    fn reads_the_mapped_address_of_the_rfc_5769_sample_response() {
        assert_eq!(transaction(&RFC_5769_RESPONSE), Some(RFC_5769_TRANSACTION));
        assert_eq!(
            mapped_address(&RFC_5769_RESPONSE, RFC_5769_TRANSACTION),
            Some("192.0.2.1:32853".parse().unwrap())
        );
        let mut other = RFC_5769_TRANSACTION;
        other[0] ^= 1;
        assert_eq!(mapped_address(&RFC_5769_RESPONSE, other), None);
    }

    #[test]
    fn a_request_and_a_response_carry_their_transaction() {
        let id = transaction_id();
        assert_eq!(transaction(&binding_request(id)), Some(id));
        let mapped = "203.0.113.7:40000".parse().unwrap();
        assert_eq!(
            mapped_address(&binding_response(id, mapped), id),
            Some(mapped)
        );
        let rtp = [
            0x80, 111, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(transaction(&rtp), None);
    }

    #[test]
    fn signs_the_rfc_5769_long_term_sample_request() {
        // Claude: the RFC's password is "TheMatrIX" after SASLprep.
        let key = long_term_key(RFC_5769_USERNAME, "example.org", "TheMatrIX");
        let request = MessageBuilder::new(
            Method::Binding,
            Class::Request,
            RFC_5769_LONG_TERM_TRANSACTION,
        )
        .attribute(Attr::Username, RFC_5769_USERNAME.as_bytes())
        .attribute(Attr::Nonce, b"f//499k954d6OL34oL9FSTvy64sA")
        .attribute(Attr::Realm, b"example.org")
        .integrity(&key);
        assert_eq!(request, RFC_5769_LONG_TERM_REQUEST);

        let message = Message::parse(&RFC_5769_LONG_TERM_REQUEST).expect("a STUN message");
        assert_eq!(
            (message.method, message.class),
            (Method::Binding, Class::Request)
        );
        assert_eq!(message.attribute(Attr::Realm), Some(&b"example.org"[..]));
        assert_eq!(message.integrity(&key), Some(true));
        let other = long_term_key(RFC_5769_USERNAME, "example.org", "TheMatrix");
        assert_eq!(message.integrity(&other), Some(false));
    }

    #[test]
    fn turn_messages_carry_their_kind_and_attributes() {
        let id = transaction_id();
        let peer: SocketAddrV4 = "192.0.2.1:32853".parse().unwrap();
        let send = MessageBuilder::new(Method::Send, Class::Indication, id)
            .address(Attr::XorPeerAddress, peer)
            .attribute(Attr::Data, b"hello")
            .finish();
        // Claude: the masked bytes RFC 5769 section 2.2 gives for this address.
        assert_eq!(
            &send[20..32],
            &[0x00, 0x12, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43]
        );
        assert_eq!(&send[..2], &[0x00, 0x16]);
        let message = Message::parse(&send).expect("a STUN message");
        assert_eq!(
            (message.method, message.class),
            (Method::Send, Class::Indication)
        );
        assert_eq!(message.address(Attr::XorPeerAddress), Some(peer));
        assert_eq!(message.attribute(Attr::Data), Some(&b"hello"[..]));

        let refused = MessageBuilder::new(Method::Allocate, Class::Error, id)
            .attribute(Attr::ErrorCode, &error_code(438))
            .attribute(Attr::Lifetime, &600u32.to_be_bytes())
            .finish();
        assert_eq!(&refused[..2], &[0x01, 0x13]);
        let message = Message::parse(&refused).expect("a STUN message");
        assert_eq!(
            (message.method, message.class),
            (Method::Allocate, Class::Error)
        );
        assert_eq!(message.error_code(), Some(438));
        assert_eq!(message.lifetime(), Some(Duration::from_secs(600)));
        assert_eq!(message.integrity(b"key"), None);

        let created = MessageBuilder::new(Method::CreatePermission, Class::Success, id).finish();
        assert_eq!(&created[..2], &[0x01, 0x08]);
        let data = MessageBuilder::new(Method::Data, Class::Indication, id).finish();
        assert_eq!(&data[..2], &[0x00, 0x17]);
    }
}
