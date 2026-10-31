use super::tests::{LATENCY, PROMPT, Renders, call_of, check_sync_mapping, settings, streaming};
use super::*;
use crate::clock::PACKET_DURATION;
use crate::packet::AIRPLAY2_PACKET_LEN;
use crate::receiver::tests::SHAIRPORT_SYNC_RAOP;
use crate::stream::Settings;
use crate::{Protocol, Status, Stream};

/// Claude: how long a test waits for a handshake, whose SRP takes a debug
/// build a few hundred milliseconds of CPU, while other suites load the
/// machine.
const HANDSHAKE: Duration = Duration::from_secs(10);

fn airplay_2() -> Settings {
    Settings {
        protocol: Some(Protocol::AirPlay2),
        ..settings()
    }
}

/// Claude: a browser that has found the fake advertising AirPlay 2, and AirPlay
/// 1 as well when `raop` says so, as the Citation does.
fn browser(fake: &Fake, raop: bool) -> Browser {
    let browser = Browser::offline();
    fake.advertise_airplay(&browser, AIRPLAY_2_RECORD);
    if raop {
        fake.advertise(&browser, SHAIRPORT_SYNC_RAOP);
    }
    browser
}

fn uris(fake: &Fake) -> Vec<String> {
    fake.requests()
        .into_iter()
        .map(|(_, _, r)| format!("{} {}", r.method, r.uri))
        .collect()
}

#[test]
fn a_stream_pairs_sets_up_records_and_plays_audio_the_receiver_opens() {
    let fake = Fake::start(Behavior::default());
    let browser = browser(&fake, false);
    let renders = Renders::default();
    let stream = Stream::start_with(&browser, fake.id(), Ok(airplay_2()), renders.renderer(None));
    assert!(wait_until(HANDSHAKE, || fake.audio().len() >= 10));
    let clock = stream.clock();
    let (before, reply, after) = fake.time(Ntp(0x0102_0304_0506_0708));
    assert_eq!(reply[..4], [0x80, 0xD3, 0x00, 0x07]);
    let received = clock.instant(Ntp::from_be_bytes(reply[16..24].try_into().unwrap()));
    assert!(before <= received + Duration::from_micros(1) && received <= after);
    stream.finish().wait();

    let requests: Vec<FakeRequest> = fake.requests().into_iter().map(|(_, _, r)| r).collect();
    assert_eq!(uris(&fake)[0], "GET /info");
    let requests = &requests[1..];
    let session_uri = &requests[2].uri;
    assert_eq!(
        uris(&fake)[1..6],
        [
            "POST /pair-setup".to_owned(),
            "POST /pair-setup".to_owned(),
            format!("SETUP {session_uri}"),
            format!("SETUP {session_uri}"),
            format!("RECORD {session_uri}"),
        ]
    );
    for pair in &requests[..2] {
        assert_eq!(pair.header("X-Apple-HKP"), Some("4"));
        assert_eq!(
            pair.header("Content-Type"),
            Some("application/octet-stream")
        );
        assert!(pair.header("User-Agent").unwrap().starts_with("AirPlay/"));
    }
    let m1 = requests[0].tlv();
    assert_eq!(
        [
            m1.get(tlv8::STATE),
            m1.get(tlv8::METHOD),
            m1.get(tlv8::FLAGS)
        ],
        [Some(&[1][..]), Some(&[0][..]), Some(&[0x10][..])]
    );
    let m3 = requests[1].tlv();
    assert_eq!(m3.get(tlv8::PUBLIC_KEY).map(<[u8]>::len), Some(384));
    assert_eq!(m3.get(tlv8::PROOF).map(<[u8]>::len), Some(64));

    let session = requests[2].plist();
    assert_eq!(session["timingProtocol"].as_string(), Some("NTP"));
    assert!(session["timingPort"].as_unsigned_integer().is_some());
    let streams = requests[3].plist();
    let setup = streams["streams"].as_array().unwrap()[0]
        .as_dictionary()
        .unwrap();
    let number = |key: &str| setup[key].as_unsigned_integer().unwrap();
    assert_eq!(
        ["type", "ct", "audioFormat", "spf", "sr"].map(number),
        [96, 2, 0x40000, 352, 44_100]
    );
    assert_eq!(setup["shk"].as_data().map(<[u8]>::len), Some(32));
    assert!(requests[4].header("RTP-Info").is_none());

    let teardowns: Vec<&FakeRequest> = requests.iter().filter(|r| r.method == "TEARDOWN").collect();
    assert_eq!(teardowns.len(), 2);
    let stream_teardown = teardowns[0].plist();
    let named = stream_teardown["streams"].as_array().unwrap()[0]
        .as_dictionary()
        .unwrap();
    assert_eq!(
        (
            named["type"].as_unsigned_integer(),
            named["streamID"].as_unsigned_integer()
        ),
        (Some(96), Some(STREAM_ID))
    );
    assert!(teardowns[1].plist().is_empty());

    let sealed = fake.audio_bytes();
    assert!(
        sealed
            .iter()
            .all(|bytes| bytes.len() == AIRPLAY2_PACKET_LEN)
    );
    let counters: Vec<u64> = sealed
        .iter()
        .map(|b| u64::from_le_bytes(b[b.len() - 8..].try_into().unwrap()))
        .collect();
    assert!(
        counters.windows(2).all(|pair| pair[1] > pair[0]),
        "{counters:?}"
    );
    let audio = fake.audio();
    assert!(audio[0].1.marker && audio[1..].iter().all(|(_, p)| !p.marker));
    for (i, (_, packet)) in audio.iter().enumerate() {
        assert!(!packet.counted, "an AirPlay 2 frame carries its count");
        assert_eq!(call_of(packet), i);
    }
}

#[test]
fn airplay_2_sync_packets_map_each_packet_to_its_play_time() {
    check_sync_mapping(Protocol::AirPlay2, None, 1000, 5_000_000);
    check_sync_mapping(
        Protocol::AirPlay2,
        Some(4_410),
        65_530,
        u32::MAX - 3 * FRAMES_PER_PACKET as u32 + 100,
    );
}

#[test]
fn retransmissions_resend_the_sealed_packets_across_the_wrap() {
    let fake = Fake::start(Behavior::default());
    let browser = browser(&fake, false);
    let settings = Settings {
        first_seq: 65_533,
        ..airplay_2()
    };
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(settings),
        Renders::default().renderer(None),
    );
    assert!(wait_until(HANDSHAKE, || fake.audio().len() >= 20));
    fake.request_retransmission(65_534, 4);
    let resent = || -> Vec<Vec<u8>> {
        fake.control()
            .into_iter()
            .filter(|b| b[1] == 0xD6)
            .collect()
    };
    assert!(wait_until(Duration::from_secs(1), || resent().len() >= 4));
    let originals = fake.audio_bytes();
    let retransmissions = resent();
    assert_eq!(retransmissions.len(), 4);
    for (retransmission, original) in retransmissions.iter().zip(&originals[1..5]) {
        assert_eq!(retransmission.len(), 4 + AIRPLAY2_PACKET_LEN);
        assert_eq!(retransmission[2..4], original[2..4]);
        assert_eq!(&retransmission[4..], original.as_slice());
    }
    let seqs: Vec<u16> = retransmissions
        .iter()
        .map(|b| decode_audio(&fake.open(&b[4..])).seq)
        .collect();
    assert_eq!(seqs, [65_534, 65_535, 0, 1]);
    let Status::Streaming(counters) = stream.status() else {
        panic!("not streaming")
    };
    assert_eq!(counters.packets_retransmitted, 4);
}

#[test]
fn the_receivers_event_requests_are_answered_on_their_own_connection() {
    let fake = Fake::start(Behavior::default());
    let browser = browser(&fake, false);
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(airplay_2()),
        Renders::default().renderer(None),
    );
    assert!(wait_until(HANDSHAKE, || streaming(&stream)));
    assert!(wait_until(Duration::from_secs(1), || fake
        .event_answers()
        .len()
        >= 2));
    let third = fake.send_event();
    assert!(wait_until(Duration::from_secs(1), || fake
        .event_answers()
        .len()
        >= 3));
    let answers = fake.event_answers();
    assert_eq!(
        answers.iter().map(Response::cseq).collect::<Vec<_>>(),
        [Some(1), Some(2), Some(third)]
    );
    for answer in &answers {
        assert_eq!(answer.status, 200);
        assert_eq!(answer.header("Audio-Latency"), Some("0"));
        assert_eq!(answer.header("Server"), Some("AirTunes/366.0"));
    }
    assert!(streaming(&stream));
}

#[test]
fn a_stream_without_its_event_connection_still_plays() {
    let fake = Fake::start(Behavior {
        events_closed: true,
        ..Behavior::default()
    });
    let browser = browser(&fake, false);
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(airplay_2()),
        Renders::default().renderer(None),
    );
    assert!(wait_until(HANDSHAKE, || fake.audio().len() >= 10));
    assert!(streaming(&stream));
}

#[test]
fn feedback_keeps_the_session_every_two_seconds() {
    let fake = Fake::start(Behavior::default());
    let browser = browser(&fake, false);
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(airplay_2()),
        Renders::default().renderer(None),
    );
    assert!(wait_until(HANDSHAKE, || streaming(&stream)));
    let feedback = || {
        fake.requests()
            .into_iter()
            .filter(|(_, _, r)| r.method == "POST" && r.uri == "/feedback")
            .map(|(at, _, _)| at)
            .collect::<Vec<_>>()
    };
    assert!(wait_until(Duration::from_secs(5), || feedback().len() >= 2));
    let at = feedback();
    let between = at[1] - at[0];
    assert!(
        between > Duration::from_millis(1500) && between < Duration::from_millis(2000) + PROMPT,
        "feedback {between:?} apart"
    );
    assert!(streaming(&stream));
}

#[test]
fn a_feedback_answered_with_an_error_still_keeps_the_session() {
    let fake = Fake::start(Behavior {
        rtsp: Rtsp::RefuseAt(Step::Feedback, 500),
        ..Behavior::default()
    });
    let browser = browser(&fake, false);
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(airplay_2()),
        Renders::default().renderer(None),
    );
    let refused = || {
        fake.requests()
            .iter()
            .filter(|(_, _, r)| r.uri == "/feedback")
            .count()
    };
    assert!(wait_until(HANDSHAKE, || refused() >= 2));
    thread::sleep(Duration::from_millis(100));
    assert!(streaming(&stream), "{:?}", stream.status());
}

#[test]
fn finishing_plays_out_then_tears_the_stream_and_the_session_down() {
    let fake = Fake::start(Behavior::default());
    let browser = browser(&fake, false);
    let renders = Renders::default();
    let stream = Stream::start_with(&browser, fake.id(), Ok(airplay_2()), renders.renderer(None));
    assert!(wait_until(HANDSHAKE, || fake.audio().len() >= 40));
    let finished_at = Instant::now();
    stream.finish().wait();
    let waited = finished_at.elapsed();
    let audio = fake.audio();
    let last = audio.iter().max_by_key(|(at, _)| *at).unwrap();
    let played_out = renders.play_at(call_of(&last.1)) + PACKET_DURATION;
    let teardowns: Vec<Instant> = fake
        .requests()
        .into_iter()
        .filter(|(_, _, r)| r.method == "TEARDOWN")
        .map(|(at, _, _)| at)
        .collect();
    assert_eq!(teardowns.len(), 2);
    assert!(
        teardowns[0] >= played_out,
        "TEARDOWN came {:?} early",
        played_out - teardowns[0]
    );
    assert!(teardowns[1] - played_out < PROMPT);
    assert!(
        waited <= LATENCY + settings().teardown_timeout,
        "finishing took {waited:?}"
    );
    assert!(wait_until(Duration::from_secs(1), || fake
        .closed(0)
        .is_some()));
}

#[test]
fn dropping_tears_down_at_once_and_the_next_stream_takes_its_turn_over_airplay_1() {
    let fake = Fake::start(Behavior::default());
    let browser = browser(&fake, true);
    let first = Stream::start_with(
        &browser,
        fake.id(),
        Ok(airplay_2()),
        Renders::default().renderer(None),
    );
    assert!(wait_until(HANDSHAKE, || fake.audio().len() >= 10));
    let next = Stream::start_with(
        &browser,
        fake.id(),
        Ok(Settings {
            protocol: Some(Protocol::AirPlay1),
            ..settings()
        }),
        Renders::default().renderer(None),
    );
    thread::sleep(Duration::from_millis(200));
    assert_eq!(next.status(), Status::Connecting);
    let dropped_at = Instant::now();
    drop(first);
    assert!(wait_until(Duration::from_millis(500), || fake
        .closed(0)
        .is_some()));
    let (teardown_at, _, _) = fake
        .requests()
        .into_iter()
        .find(|(_, _, r)| r.method == "TEARDOWN")
        .unwrap();
    assert!(teardown_at - dropped_at < PROMPT);
    assert!(wait_until(HANDSHAKE, || streaming(&next)));
    let (next_at, _, request) = fake
        .requests()
        .into_iter()
        .find(|(_, connection, _)| *connection == 1)
        .unwrap();
    assert_eq!(request.method, "OPTIONS");
    let last_teardown = fake
        .requests()
        .into_iter()
        .filter(|(_, connection, r)| *connection == 0 && r.method == "TEARDOWN")
        .map(|(at, _, _)| at)
        .max()
        .unwrap();
    assert!(
        next_at > last_teardown,
        "the next stream began before the first was torn down"
    );
}

#[test]
fn finishing_or_dropping_at_each_handshake_step_is_prompt() {
    for step in [
        Step::PairM1,
        Step::PairM3,
        Step::SessionSetup,
        Step::StreamSetup,
        Step::Record,
    ] {
        let fake = Fake::start(Behavior {
            rtsp: Rtsp::HangAt(step),
            ..Behavior::default()
        });
        let browser = browser(&fake, true);
        let hanging = || Settings {
            answer_timeout: Duration::from_secs(5),
            ..airplay_2()
        };
        let reached = |connection: usize| {
            fake.requests()
                .iter()
                .any(|(_, c, r)| *c == connection && step_of(r) == Some(step))
        };
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(hanging()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(HANDSHAKE, || reached(0)), "{step:?}");
        let started = Instant::now();
        stream.finish().wait();
        assert!(
            started.elapsed() < PROMPT,
            "{step:?}: finish took {:?}",
            started.elapsed()
        );

        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(hanging()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(HANDSHAKE, || reached(1)), "{step:?}");
        let dropped_at = Instant::now();
        drop(stream);
        assert!(wait_until(Duration::from_secs(1), || fake
            .closed(1)
            .is_some()));
        assert!(fake.closed(1).unwrap() - dropped_at < PROMPT, "{step:?}");
        assert!(
            !uris(&fake)
                .iter()
                .any(|r| r.starts_with("OPTIONS") || r.starts_with("ANNOUNCE")),
            "{step:?}: a cancelled stream fell back"
        );
    }
}

#[test]
fn a_stream_setup_left_unanswered_tears_down_within_its_bound_and_is_unreachable() {
    let fake = Fake::start(Behavior {
        rtsp: Rtsp::HangAt(Step::StreamSetup),
        ..Behavior::default()
    });
    let browser = browser(&fake, true);
    let settings = settings();
    let bound = settings.answer_timeout + settings.teardown_timeout;
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(settings),
        Renders::default().renderer(None),
    );
    assert!(wait_until(HANDSHAKE, || fake
        .requests()
        .iter()
        .any(|(_, _, r)| step_of(r) == Some(Step::StreamSetup))));
    let hung_at = Instant::now();
    assert!(wait_until(bound + Duration::from_secs(1), || stream
        .status()
        != Status::Connecting));
    let waited = hung_at.elapsed();
    let Status::Unreachable(reason) = stream.status() else {
        panic!("{:?}", stream.status())
    };
    assert!(reason.contains("no answer to SETUP"), "{reason}");
    assert!(waited < bound + PROMPT, "unreachable after {waited:?}");
    assert_eq!(fake.methods().last().map(String::as_str), Some("TEARDOWN"));
    assert!(
        !fake.methods().contains(&"OPTIONS".to_owned()),
        "a receiver that stopped answering was tried again over AirPlay 1"
    );
}

#[test]
fn a_wrong_pairing_proof_is_never_trusted_or_fallen_back_from() {
    let fake = Fake::start(Behavior {
        pairing: Pairing::WrongProof,
        ..Behavior::default()
    });
    let browser = browser(&fake, true);
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(settings()),
        Renders::default().renderer(None),
    );
    assert!(wait_until(HANDSHAKE, || stream.status() != Status::Connecting));
    let Status::Unreachable(reason) = stream.status() else {
        panic!("{:?}", stream.status())
    };
    assert!(reason.contains("pairing proof is wrong"), "{reason}");
    assert_eq!(
        uris(&fake),
        ["GET /info", "POST /pair-setup", "POST /pair-setup"]
    );
    assert!(wait_until(Duration::from_secs(1), || fake
        .closed(0)
        .is_some()));
}

#[test]
fn a_refused_pairing_falls_back_to_airplay_1_unless_airplay_2_was_asked_for() {
    for behavior in [
        Behavior {
            rtsp: Rtsp::RefuseAt(Step::PairM1, 470),
            ..Behavior::default()
        },
        Behavior {
            pairing: Pairing::ErrorAtM2(6),
            ..Behavior::default()
        },
    ] {
        let fake = Fake::start(behavior);
        let both = browser(&fake, true);
        let stream = Stream::start_with(
            &both,
            fake.id(),
            Ok(settings()),
            Renders::default().renderer(None),
        );
        assert!(
            wait_until(HANDSHAKE, || streaming(&stream)),
            "{:?}",
            stream.status()
        );
        let methods: Vec<(usize, String)> = fake
            .requests()
            .into_iter()
            .map(|(_, c, r)| {
                (
                    c,
                    format!("{} {}", r.method, r.uri.split('/').nth(1).unwrap_or("")),
                )
            })
            .collect();
        assert_eq!(
            methods[..2],
            [
                (0, "GET info".to_owned()),
                (0, "POST pair-setup".to_owned())
            ]
        );
        assert_eq!(methods[2].0, 1, "AirPlay 1 runs on a new connection");
        assert!(methods[2].1.starts_with("OPTIONS"));
        assert!(fake.closed(0).is_some());
        drop(stream);

        let forced = Fake::start(behavior);
        let both = browser(&forced, true);
        let stream = Stream::start_with(
            &both,
            forced.id(),
            Ok(airplay_2()),
            Renders::default().renderer(None),
        );
        assert!(wait_until(HANDSHAKE, || stream.status() != Status::Connecting));
        let Status::Unreachable(reason) = stream.status() else {
            panic!("{:?}", stream.status())
        };
        assert!(reason.starts_with("fake: it "), "{reason}");
        assert!(
            uris(&forced)
                .iter()
                .all(|r| r == "GET /info" || r == "POST /pair-setup")
        );
    }
}

#[test]
fn a_fallback_reaches_an_airplay_1_service_found_after_the_stream_started() {
    let fake = Fake::start(Behavior {
        rtsp: Rtsp::RefuseAt(Step::SessionSetup, 400),
        ..Behavior::default()
    });
    let browser = browser(&fake, false);
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(settings()),
        Renders::default().renderer(None),
    );
    assert!(wait_until(HANDSHAKE, || fake
        .requests()
        .iter()
        .any(|(_, _, r)| r.method == "POST")));
    fake.advertise(&browser, SHAIRPORT_SYNC_RAOP);
    assert!(
        wait_until(HANDSHAKE, || streaming(&stream)),
        "{:?}",
        stream.status()
    );
    assert!(fake.methods().contains(&"ANNOUNCE".to_owned()));
}

#[test]
fn a_refused_stream_setup_tears_the_session_down_before_falling_back() {
    let fake = Fake::start(Behavior {
        rtsp: Rtsp::RefuseAt(Step::StreamSetup, 400),
        ..Behavior::default()
    });
    let browser = browser(&fake, true);
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(settings()),
        Renders::default().renderer(None),
    );
    assert!(
        wait_until(HANDSHAKE, || streaming(&stream)),
        "{:?}",
        stream.status()
    );
    let requests: Vec<(usize, String)> = fake
        .requests()
        .into_iter()
        .map(|(_, c, r)| (c, r.method))
        .collect();
    let expected = [
        (0, "GET"),
        (0, "POST"),
        (0, "POST"),
        (0, "SETUP"),
        (0, "SETUP"),
        (0, "TEARDOWN"),
        (1, "OPTIONS"),
        (1, "ANNOUNCE"),
        (1, "SETUP"),
        (1, "RECORD"),
    ];
    assert_eq!(requests[..10], expected.map(|(c, m)| (c, m.to_owned())));
    let teardown = fake.requests()[5].2.plist();
    assert!(teardown.is_empty(), "the TEARDOWN ends the whole session");
    assert!(fake.requests()[5].0 < fake.requests()[6].0);
    assert!(
        fake.audio().iter().all(|(_, p)| p.counted),
        "the audio is AirPlay 1's"
    );
}

#[test]
fn a_closed_connection_loses_an_airplay_2_stream() {
    let fake = Fake::start(Behavior::default());
    let browser = browser(&fake, false);
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(airplay_2()),
        Renders::default().renderer(None),
    );
    assert!(wait_until(HANDSHAKE, || streaming(&stream)));
    fake.close_connections();
    assert!(wait_until(Duration::from_secs(1), || !streaming(&stream)));
    assert_eq!(
        stream.status(),
        Status::Lost("the receiver closed the RTSP connection".into())
    );
}
