use std::net::{Ipv4Addr, SocketAddrV4};

const HEADER_LEN: usize = 20;
const MAGIC_COOKIE: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const MAPPED_ADDRESS: u16 = 0x0001;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;
const IPV4: u8 = 0x01;

pub type Transaction = [u8; 12];

pub fn transaction_id() -> Transaction {
    let mut transaction = [0; 12];
    getrandom::fill(&mut transaction).expect("system randomness");
    transaction
}

pub fn binding_request(transaction: Transaction) -> [u8; HEADER_LEN] {
    let mut request = [0; HEADER_LEN];
    request[..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    request[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    request[8..].copy_from_slice(&transaction);
    request
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
    let (header, mut attributes) = response.split_first_chunk::<HEADER_LEN>()?;
    if u16::from_be_bytes([header[0], header[1]]) != BINDING_SUCCESS
        || usize::from(u16::from_be_bytes([header[2], header[3]])) != attributes.len()
        || header[4..8] != MAGIC_COOKIE.to_be_bytes()
        || header[8..] != transaction
    {
        return None;
    }
    let mut mapped = None;
    while let Some((head, rest)) = attributes.split_first_chunk::<4>() {
        let kind = u16::from_be_bytes([head[0], head[1]]);
        let length = usize::from(u16::from_be_bytes([head[2], head[3]]));
        let value = rest.get(..length)?;
        match kind {
            XOR_MAPPED_ADDRESS => {
                if let Some(address) = address(value) {
                    return Some(unxor(address));
                }
            }
            MAPPED_ADDRESS => mapped = mapped.or_else(|| address(value)),
            _ => {}
        }
        attributes = rest.get(length.next_multiple_of(4)..)?;
    }
    mapped
}

fn address(value: &[u8]) -> Option<SocketAddrV4> {
    let &[_, IPV4, port_high, port_low, a, b, c, d] = value else {
        return None;
    };
    Some(SocketAddrV4::new(
        Ipv4Addr::new(a, b, c, d),
        u16::from_be_bytes([port_high, port_low]),
    ))
}

/// Claude: XOR-MAPPED-ADDRESS and its plain form differ only by this mask, so
/// it both encodes and decodes.
fn unxor(address: SocketAddrV4) -> SocketAddrV4 {
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
        let xored = unxor(mapped);
        let mut response = Vec::with_capacity(HEADER_LEN + 12);
        response.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        response.extend_from_slice(&12u16.to_be_bytes());
        response.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        response.extend_from_slice(&transaction);
        response.extend_from_slice(&XOR_MAPPED_ADDRESS.to_be_bytes());
        response.extend_from_slice(&8u16.to_be_bytes());
        response.extend_from_slice(&[0, IPV4]);
        response.extend_from_slice(&xored.port().to_be_bytes());
        response.extend_from_slice(&xored.ip().octets());
        response
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
}
