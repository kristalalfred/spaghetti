use super::*;

const MAC: ClockIdentity = ClockIdentity([0x46, 0xe2, 0x2c, 0xb5, 0x2f, 0xe7, 0, 8]);
const BAR: ClockIdentity = ClockIdentity([0xb8, 0xab, 0x62, 0xff, 0xfe, 0xc6, 0x1a, 0xdf]);
const MAC_PORT: PortIdentity = PortIdentity {
    clock: MAC,
    port: 32777,
};
const BAR_PORT: PortIdentity = PortIdentity {
    clock: BAR,
    port: 32768,
};

fn bytes(hex: &str) -> Vec<u8> {
    hex.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
        .collect()
}

fn golden(hex: &str, message: Message) {
    let wire = bytes(hex);
    assert_eq!(Message::decode(&wire), Some(message.clone()));
    assert_eq!(message.encode(), wire);
    for length in 0..wire.len() {
        assert!(Message::decode(&wire[..length]).is_none());
    }
}

#[test]
fn capture_mac_announce_and_ipv6_path() {
    let mut message = Message::new(
        MAC_PORT,
        1,
        -2,
        Body::Announce {
            origin: Timestamp::ZERO,
            utc_offset: 37,
            reserved: 0,
            dataset: Dataset::local(MAC, 250),
            time_source: 0xa0,
        },
    );
    message.tlvs = vec![Tlv::Path(vec![MAC])];
    golden(
        "1b02004c0000040800000000000000000000000046e22cb52fe700088009000105fe00000000000000000000002500faf821436af746e22cb52fe700080000a00008000846e22cb52fe70008",
        message.clone(),
    );
    message.source.port = 32778;
    golden(
        "1b02004c0000040800000000000000000000000046e22cb52fe70008800a000105fe00000000000000000000002500faf821436af746e22cb52fe700080000a00008000846e22cb52fe70008",
        message,
    );
}

#[test]
fn capture_mac_signaling_sync_follow_up_and_delay_request() {
    let mut signaling = Message::new(
        MAC_PORT,
        0,
        -128,
        Body::Signaling {
            target: PortIdentity {
                clock: ClockIdentity([0; 8]),
                port: 0,
            },
        },
    );
    signaling.tlvs = apple_signaling();
    golden(
        "1c02006a0000040800000000000000000000000046e22cb52fe700088009000005800000000000000000000000030016000d930000010003000000002710000027100000000000030020000d93000005000f000000000000271000002710000027100000271000000000",
        signaling,
    );
    golden(
        "1002002c0000060800000000000000000000000046e22cb52fe700088009000100fd00000000000000000000",
        Message::new(MAC_PORT, 1, -3, Body::Sync(Timestamp::ZERO)),
    );
    let mut follow = Message::new(
        MAC_PORT,
        1,
        -3,
        Body::FollowUp(Timestamp {
            seconds: 636538,
            nanos: 113014625,
        }),
    );
    follow.correction = 77791 * 65536;
    follow.tlvs = vec![
        Tlv::Follow(FollowInfo {
            rate_offset: 0,
            timebase: 0,
            phase_change: [0, 0, 0, 0, 0, 1, 0x70, 0xd1, 0xae, 0xe8, 0xb0, 0xad],
            frequency_change: 0x02e2b469,
        }),
        Tlv::Organization {
            oui: [0, 0x0d, 0x93],
            subtype: [0, 0, 4],
            data: vec![0x46, 0xe2, 0x2c, 0xb5, 0x2f, 0xe7, 0, 8, 0, 0],
        },
    ];
    golden(
        "1802006000000408000000012fdf00000000000046e22cb52fe700088009000102fd00000009b67a06bc77610003001c0080c200000100000000000000000000000170d1aee8b0ad02e2b46900030010000d9300000446e22cb52fe700080000",
        follow,
    );
    golden(
        "1102002c0000040800000000000000000000000046e22cb52fe700088009000100fd00000000000000000000",
        Message::new(MAC_PORT, 1, -3, Body::DelayReq(Timestamp::ZERO)),
    );
}

#[test]
fn capture_soundbar_messages() {
    let mut announce = Message::new(
        BAR_PORT,
        0,
        0,
        Body::Announce {
            origin: Timestamp::ZERO,
            utc_offset: 0,
            reserved: 0,
            dataset: Dataset {
                priority1: 248,
                class: 248,
                accuracy: 0xfe,
                variance: 17258,
                priority2: 248,
                grandmaster: BAR,
                steps: 0,
            },
            time_source: 0xa0,
        },
    );
    announce.flags = 0x0600;
    announce.tlvs = vec![Tlv::Path(vec![BAR])];
    golden(
        "1b02004c00000600000000000000000000000000b8ab62fffec61adf80000000050000000000000000000000000000f8f8fe436af8b8ab62fffec61adf0000a000080008b8ab62fffec61adf",
        announce,
    );
    let mut signaling = Message::new(
        BAR_PORT,
        0,
        127,
        Body::Signaling {
            target: PortIdentity {
                clock: ClockIdentity([0; 8]),
                port: 0,
            },
        },
    );
    signaling.flags = 0x0600;
    signaling.tlvs = vec![Tlv::Interval {
        link: 0,
        sync: -3,
        announce: -128,
        flags: 3,
        reserved: 0,
    }];
    golden(
        "1c02003c00000600000000000000000000000000b8ab62fffec61adf80000000057f000000000000000000000003000c0080c200000200fd80030000",
        signaling,
    );
    let mut sync = Message::new(
        BAR_PORT,
        0,
        -3,
        Body::Sync(Timestamp {
            seconds: 231017,
            nanos: 806449999,
        }),
    );
    sync.flags = 0x0600;
    golden(
        "1002002c00000600000000000000000000000000b8ab62fffec61adf8000000000fd0000000386693011734f",
        sync,
    );
    let mut follow = Message::new(
        BAR_PORT,
        0,
        -3,
        Body::FollowUp(Timestamp {
            seconds: 231017,
            nanos: 856645460,
        }),
    );
    follow.flags = 0x0600;
    follow.correction = 91923 * 65536;
    follow.tlvs = vec![Tlv::Follow(FollowInfo {
        rate_offset: 0,
        timebase: 2,
        phase_change: [0; 12],
        frequency_change: 0,
    })];
    golden(
        "1802004c00000600000000016713000000000000b8ab62fffec61adf8000000002fd000000038669330f5f540003001c0080c200000100000000000200000000000000000000000000000000",
        follow,
    );
    let mut response = Message::new(
        BAR_PORT,
        1,
        0,
        Body::DelayResp {
            received: Timestamp {
                seconds: 231017,
                nanos: 867152306,
            },
            requester: MAC_PORT,
        },
    );
    response.flags = 0x0600;
    response.correction = 141846 * 65536;
    golden(
        "1902003600000600000000022a16000000000000b8ab62fffec61adf80000001030000000003866933afb1b246e22cb52fe700088009",
        response,
    );
}

#[test]
fn invalid_headers_timestamps_and_tlv_lengths_are_rejected() {
    let valid = Message::new(MAC_PORT, 1, -3, Body::Sync(Timestamp::ZERO)).encode();
    for (index, value) in [(0, 0), (1, 3), (4, 1), (5, 1), (6, 0)] {
        let mut invalid = valid.clone();
        invalid[index] = value;
        assert!(Message::decode(&invalid).is_none());
    }
    let mut invalid = valid;
    invalid[40..44].copy_from_slice(&1_000_000_000_u32.to_be_bytes());
    assert!(Message::decode(&invalid).is_none());
    let mut message = Message::new(MAC_PORT, u16::MAX, -3, Body::FollowUp(Timestamp::ZERO));
    message.correction = -65537;
    message.tlvs.push(Tlv::Unknown {
        kind: 0x1234,
        data: vec![1, 2],
    });
    assert_eq!(Message::decode(&message.encode()), Some(message.clone()));
    message.tlvs = vec![Tlv::Unknown {
        kind: 8,
        data: vec![0, 0],
    }];
    assert!(Message::decode(&message.encode()).is_none());
    message.tlvs = vec![Tlv::Unknown {
        kind: 3,
        data: vec![0, 0],
    }];
    assert!(Message::decode(&message.encode()).is_none());
}
