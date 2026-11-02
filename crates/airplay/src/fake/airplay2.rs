use super::tests::{LATENCY, PROMPT, Renders, call_of, check_sync_mapping, settings, streaming};
use super::*;
use crate::clock::PACKET_DURATION;
use crate::packet::AIRPLAY2_PACKET_LEN;
use crate::receiver::tests::{MOVE_2_RAOP, SHAIRPORT_SYNC_RAOP};
use crate::stream::Settings;
use crate::{Protocol, Status, Stream};
use std::io::Write;

/// Claude: how long a test waits for a handshake, whose SRP takes a debug
/// build a few hundred milliseconds of CPU, while other suites load the
/// machine.
const HANDSHAKE: Duration = Duration::from_secs(10);

/// Claude: the suite's settings with room for the fake's SRP in a debug build
/// to answer pairing, for a stream that picks its protocol.
fn automatic() -> Settings {
    Settings {
        answer_timeout: Duration::from_secs(3),
        ..settings()
    }
}

fn airplay_2() -> Settings {
    Settings {
        protocol: Some(Protocol::AirPlay2),
        ..automatic()
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

/// Claude: whether the fake has seen both TEARDOWNs, which may come after a
/// bounded `Finishing::wait` returns on a loaded machine.
fn torn_down(fake: &Fake) -> bool {
    fake.methods().iter().filter(|m| *m == "TEARDOWN").count() == 2
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
    stream.finish(Duration::ZERO).wait();
    assert!(wait_until(Duration::from_secs(1), || torn_down(&fake)));

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

/// Claude: after RECORD, and before any audio or sync packet, the stream
/// flushes to its first packet, naming its sequence number and RTP time.
#[test]
fn a_flush_naming_the_first_packet_follows_record_before_any_packet() {
    let fake = Fake::start(Behavior::default());
    let browser = browser(&fake, false);
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(airplay_2()),
        Renders::default().renderer(None),
    );
    assert!(wait_until(HANDSHAKE, || {
        fake.audio().len() >= 10 && !fake.syncs().is_empty()
    }));
    drop(stream);

    let requests: Vec<FakeRequest> = fake.requests().into_iter().map(|(_, _, r)| r).collect();
    let record = requests.iter().position(|r| r.method == "RECORD").unwrap();
    let flush = &requests[record + 1];
    assert_eq!(flush.method, "FLUSH");
    assert_eq!(flush.uri, requests[record].uri);
    let first = &fake.audio()[0].1;
    assert_eq!(flush.header("Range"), Some("npt=0-"));
    assert_eq!(
        flush.header("RTP-Info"),
        Some(format!("seq={};rtptime={}", first.seq, first.timestamp).as_str())
    );
    let seen = fake.seen();
    let position = |wanted: &dyn Fn(&Seen) -> bool| seen.iter().position(wanted).unwrap();
    let flushed =
        position(&|s| matches!(s, Seen::Request { request, .. } if request.method == "FLUSH"));
    assert!(flushed < position(&|s| matches!(s, Seen::Audio { .. })));
    assert!(flushed < position(&|s| matches!(s, Seen::Control { .. })));
}

/// Claude: the stream's first packet plays one latency after the receiver
/// answers its FLUSH: here 200 ms after the FLUSH arrived, and so 200 ms
/// after RECORD was answered.
#[test]
fn the_first_packet_plays_one_latency_after_the_flush_is_answered() {
    let slow = Duration::from_millis(200);
    let fake = Fake::start(Behavior {
        rtsp: Rtsp::SlowAt(Step::Flush, slow),
        ..Behavior::default()
    });
    let browser = browser(&fake, false);
    let renders = Renders::default();
    let stream = Stream::start_with(&browser, fake.id(), Ok(airplay_2()), renders.renderer(None));
    assert!(wait_until(HANDSHAKE, || renders.count() >= 1));
    drop(stream);

    let (flushed, _, _) = fake
        .requests()
        .into_iter()
        .find(|(_, _, r)| r.method == "FLUSH")
        .unwrap();
    let after_answer = renders
        .play_at(0)
        .checked_duration_since(flushed + slow)
        .expect("the first packet plays after the FLUSH is answered");
    assert!(
        after_answer >= LATENCY && after_answer < LATENCY + Duration::from_millis(100),
        "{after_answer:?}"
    );
}

/// Claude: a receiver that refuses the FLUSH, or never answers it, still
/// gets the stream: at once, or once the answer timeout passes.
#[test]
fn a_refused_or_unanswered_flush_still_streams() {
    for rtsp in [
        Rtsp::RefuseAt(Step::Flush, 501),
        Rtsp::IgnoreAt(Step::Flush),
    ] {
        let fake = Fake::start(Behavior {
            rtsp,
            ..Behavior::default()
        });
        let browser = browser(&fake, false);
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(airplay_2()),
            Renders::default().renderer(None),
        );
        assert!(
            wait_until(HANDSHAKE, || fake.audio().len() >= 10),
            "{rtsp:?}"
        );
        assert!(streaming(&stream), "{rtsp:?}: {:?}", stream.status());
        stream.finish(Duration::ZERO).wait();
        assert!(fake.methods().contains(&"FLUSH".to_owned()), "{rtsp:?}");
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
        event_port: EventPort::Closed,
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
fn finishing_with_playout_delay_preserves_the_tail() {
    super::tests::check_finish_playout_delay(Protocol::AirPlay2);
}

#[test]
fn finishing_plays_out_then_tears_the_stream_and_the_session_down() {
    let fake = Fake::start(Behavior::default());
    let browser = browser(&fake, false);
    let renders = Renders::default();
    let stream = Stream::start_with(&browser, fake.id(), Ok(airplay_2()), renders.renderer(None));
    assert!(wait_until(HANDSHAKE, || fake.audio().len() >= 40));
    let finished_at = Instant::now();
    stream.finish(Duration::ZERO).wait();
    let waited = finished_at.elapsed();
    assert!(wait_until(Duration::from_secs(1), || torn_down(&fake)));
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
        Step::Flush,
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
        stream.finish(Duration::ZERO).wait();
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
        let closed_after = fake.closed(1).unwrap() - dropped_at;
        assert!(
            closed_after < PROMPT,
            "{step:?}: closed {closed_after:?} after the drop"
        );
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
    let settings = automatic();
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
fn a_session_setup_answer_without_an_event_port_tears_the_session_down() {
    let fake = Fake::start(Behavior {
        event_port: EventPort::Missing,
        ..Behavior::default()
    });
    let browser = browser(&fake, true);
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(automatic()),
        Renders::default().renderer(None),
    );
    assert!(wait_until(HANDSHAKE, || stream.status() != Status::Connecting));
    assert_eq!(
        stream.status(),
        Status::Unreachable("fake: its SETUP answer has no eventPort".into())
    );
    let methods: Vec<String> = fake.methods();
    assert_eq!(methods[methods.len() - 2..], ["SETUP", "TEARDOWN"]);
    assert!(
        fake.requests()
            .iter()
            .all(|(_, connection, _)| *connection == 0)
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
        Ok(automatic()),
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
            Ok(automatic()),
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
        Ok(automatic()),
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
fn an_automatic_stream_takes_airplay_2_whichever_service_resolves_first() {
    for raop_first in [true, false] {
        let raop = Fake::start(Behavior::default());
        let airplay = Fake::start(Behavior::default());
        let browser = Browser::offline();
        let advertise = |raop_next: bool| {
            if raop_next {
                raop.advertise(&browser, MOVE_2_RAOP);
            } else {
                airplay.advertise_airplay(&browser, AIRPLAY_2_RECORD);
            }
        };
        advertise(raop_first);
        let stream = Stream::start_with(
            &browser,
            raop.id(),
            Ok(automatic()),
            Renders::default().renderer(None),
        );
        thread::sleep(Duration::from_millis(100));
        advertise(!raop_first);
        assert!(
            wait_until(HANDSHAKE, || streaming(&stream)),
            "{:?} with _raop first: {raop_first}",
            stream.status()
        );
        assert_eq!(raop.accepted(), 0, "_raop first: {raop_first}");
        assert_eq!(
            uris(&airplay)[..2],
            ["GET /info", "POST /pair-setup"],
            "_raop first: {raop_first}"
        );
    }
}

#[test]
fn a_receiver_whose_raop_record_offers_airplay_2_waits_for_it_within_the_lookup_window() {
    let lookup = automatic().lookup_window;
    let raop = Fake::start(Behavior::default());
    let browser = raop.browser(MOVE_2_RAOP);
    let started = Instant::now();
    let stream = Stream::start_with(
        &browser,
        raop.id(),
        Ok(automatic()),
        Renders::default().renderer(None),
    );
    assert!(wait_until(HANDSHAKE, || streaming(&stream)));
    let (first_at, _, request) = raop.requests()[0].clone();
    assert_eq!(request.method, "OPTIONS");
    let waited = first_at - started;
    assert!(
        waited >= lookup && waited < lookup + PROMPT,
        "AirPlay 1 began after {waited:?}"
    );

    let classic = Fake::start(Behavior::default());
    let browser = classic.browser(SHAIRPORT_SYNC_RAOP);
    let started = Instant::now();
    let stream = Stream::start_with(
        &browser,
        classic.id(),
        Ok(automatic()),
        Renders::default().renderer(None),
    );
    assert!(wait_until(HANDSHAKE, || streaming(&stream)));
    let waited = classic.requests()[0].0 - started;
    assert!(waited < PROMPT, "AirPlay 1 began after {waited:?}");
}

#[test]
fn an_airplay_service_resolved_while_a_stream_waits_its_turn_is_taken() {
    let raop = Fake::start(Behavior::default());
    let airplay = Fake::start(Behavior::default());
    let browser = raop.browser(MOVE_2_RAOP);
    let first = Stream::start_with(
        &browser,
        raop.id(),
        Ok(automatic()),
        Renders::default().renderer(None),
    );
    assert!(wait_until(HANDSHAKE, || streaming(&first)));
    let next = Stream::start_with(
        &browser,
        raop.id(),
        Ok(automatic()),
        Renders::default().renderer(None),
    );
    thread::sleep(automatic().lookup_window + Duration::from_millis(200));
    assert_eq!(next.status(), Status::Connecting);
    airplay.advertise_airplay(&browser, AIRPLAY_2_RECORD);
    first.finish(Duration::ZERO).wait();
    assert!(
        wait_until(HANDSHAKE, || streaming(&next)),
        "{:?}",
        next.status()
    );
    assert_eq!(raop.accepted(), 1);
    assert_eq!(uris(&airplay)[0], "GET /info");
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
        Ok(automatic()),
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

/// Claude: collects what the log writes, for the test below.
#[derive(Clone)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn the_debug_log_of_a_whole_session_holds_no_key_or_pairing_value() {
    let log = Log(Arc::default());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer({
            let log = log.clone();
            move || log.clone()
        })
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let fake = Fake::start(Behavior::default());
    let browser = browser(&fake, false);
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(airplay_2()),
        Renders::default().renderer(None),
    );
    assert!(wait_until(HANDSHAKE, || fake.audio().len() >= 10));
    fake.send_event();
    assert!(wait_until(HANDSHAKE, || fake.event_answers().len() >= 3));
    stream.finish(Duration::ZERO).wait();

    let text = String::from_utf8_lossy(&log.0.lock().unwrap()).into_owned();
    assert!(text.contains("SETUP"), "the session was logged");
    assert!(
        text.contains("shk: <32 bytes>"),
        "the key was logged as its length"
    );
    let requests = fake.requests();
    let m3 = requests
        .iter()
        .find(|(_, _, r)| r.uri == "/pair-setup" && r.tlv().get(tlv8::PROOF).is_some())
        .unwrap()
        .2
        .tlv();
    let server = transient_server();
    let audio_key = fake.state.audio_key.lock().unwrap().unwrap();
    let sealed = fake.audio_bytes()[0].clone();
    let secrets: [(&str, &[u8]); 6] = [
        ("shk", &audio_key),
        ("A", m3.get(tlv8::PUBLIC_KEY).unwrap()),
        ("M1", m3.get(tlv8::PROOF).unwrap()),
        ("B", &server.public),
        ("salt", &server.salt),
        ("sealed audio", &sealed[12..40]),
    ];
    for (name, secret) in secrets {
        let lower: String = secret[..8].iter().map(|b| format!("{b:02x}")).collect();
        let listed = format!("{:?}", &secret[..4])
            .trim_end_matches(']')
            .to_owned();
        assert!(
            !text.to_ascii_lowercase().contains(&lower) && !text.contains(&listed),
            "the log shows {name}"
        );
    }
}
