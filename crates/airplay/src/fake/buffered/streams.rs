use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plist::Dictionary;

use super::super::tests::{LATENCY, PROMPT, expected_frame, settings, streaming};
use super::super::{
    BUFFERED_AIRPLAY_2_RECORD, Behavior, Fake, FakeRequest, Reading, Rtsp, Step, wait_until,
};
use super::BufferedPacket;
use crate::clock::{PACKET_DURATION, span};
use crate::packet::{BUFFERED_PACKET_LEN, BUFFERED_SEQUENCES};
use crate::ptp::{Config, Ptp, PtpTime, Timebase, Timeline, Timing};
use crate::receiver::tests::SHAIRPORT_SYNC_RAOP;
use crate::stream::Settings;
use crate::{Browser, FRAMES_PER_PACKET, Protocol, Render, Status, Stream};

/// Claude: how long a test waits for a buffered stream to start: pairing,
/// which a debug build's SRP takes a few hundred milliseconds over, and
/// timing, which settles no sooner than the leading clock's two-second
/// election, then a second of exchanges for a follower.
const STARTING: Duration = Duration::from_secs(15);
/// Claude: the priority a Mac announces. A receiver announcing 248, as the
/// soundbar does, leads it; one announcing worse is led.
const SENDER: u8 = 250;
const LEADING: u8 = 248;
const FOLLOWING: u8 = 252;
/// Claude: how far a packet's play time as the stream gave it may lie from
/// when the receiver plays it, both reading PTP over loopback.
const TIMED_WITHIN: Duration = Duration::from_millis(2);

fn localhost() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

fn clock(priority1: u8) -> Arc<Ptp> {
    Ptp::bind(Config {
        event_port: 0,
        general_port: 0,
        priority1,
    })
    .unwrap()
}

/// Claude: a fake receiver taking buffered AirPlay 2 with a clock of
/// `priority1`, and the settings of a stream timing with it from a clock of
/// its own.
struct Setup {
    fake: Fake,
    browser: Browser,
    sender: Arc<Ptp>,
    receiver: Arc<Ptp>,
    settings: Settings,
}

fn setup(behavior: Behavior, priority1: u8) -> Setup {
    let fake = Fake::start(behavior);
    let browser = Browser::offline();
    fake.advertise_airplay(&browser, BUFFERED_AIRPLAY_2_RECORD);
    let (sender, receiver) = (clock(SENDER), clock(priority1));
    fake.time_with(receiver.clone(), sender.endpoint(localhost()));
    let settings = Settings {
        protocol: Some(Protocol::AirPlay2),
        answer_timeout: Duration::from_secs(3),
        timing: Some(Ok((sender.clone(), receiver.endpoint(localhost())))),
        ..settings()
    };
    Setup {
        fake,
        browser,
        sender,
        receiver,
        settings,
    }
}

/// Claude: what the stream asked its renderer for, call by call: when it
/// called and the play time it gave. Each packet carries its call number in
/// its first frame.
#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<(Instant, Instant)>>>);

impl Calls {
    fn renderer(&self) -> impl FnMut(Render<'_>) + Send + 'static {
        let calls = self.0.clone();
        move |render: Render<'_>| {
            let called_at = Instant::now();
            let n = {
                let mut calls = calls.lock().unwrap();
                calls.push((called_at, render.play_at));
                calls.len() - 1
            };
            let [left, right] = render.channels;
            for i in 0..FRAMES_PER_PACKET {
                let [l, r] = expected_frame(n, i);
                left[i] = f32::from(l) / 32768.0;
                right[i] = f32::from(r) / 32768.0;
            }
        }
    }

    fn all(&self) -> Vec<(Instant, Instant)> {
        self.0.lock().unwrap().clone()
    }
}

fn call_of(packet: &BufferedPacket) -> usize {
    let call = packet.frames[0][0] as u16 as usize;
    for (i, frame) in packet.frames.iter().enumerate() {
        assert_eq!(*frame, expected_frame(call, i), "frame {i} of call {call}");
    }
    call
}

fn integer(dictionary: &Dictionary, key: &str) -> u64 {
    dictionary[key]
        .as_signed_integer()
        .map(|value| value as u64)
        .or_else(|| dictionary[key].as_unsigned_integer())
        .unwrap_or_else(|| panic!("{key} is an integer"))
}

/// Claude: the anchor the fake accepted, its last.
fn accepted(fake: &Fake) -> Dictionary {
    fake.anchors().last().expect("an anchor").1.clone()
}

/// Claude: when the fake plays `packet`, by the anchor it accepted, as
/// shairport-sync reads one, and its own timing with the sender.
fn plays_at(fake: &Fake, packet: &BufferedPacket) -> Instant {
    let anchor = accepted(fake);
    let fraction =
        (u128::from(integer(&anchor, "networkTimeFrac")) * 1_000_000_000).div_ceil(1 << 64);
    let network = PtpTime {
        nanos: integer(&anchor, "networkTimeSecs") * 1_000_000_000 + fraction as u64,
    };
    let frames = packet
        .timestamp
        .wrapping_sub(integer(&anchor, "rtpTime") as u32);
    let at = network.after(span(u64::from(frames))).unwrap();
    let Some(Timing::Settled(timebase)) = fake.timing() else {
        panic!("the fake's timing is {:?}", fake.timing())
    };
    timebase.host(at).unwrap()
}

fn off(a: Instant, b: Instant) -> Duration {
    a.max(b) - a.min(b)
}

fn requests(fake: &Fake) -> Vec<FakeRequest> {
    fake.requests().into_iter().map(|(_, _, r)| r).collect()
}

/// Claude: checks that the fake got the packets the renderer was asked for,
/// in order and whole, each rendered `LATENCY` before the play time the
/// stream gave it, and played by the fake at that time.
fn check_played(fake: &Fake, calls: &Calls) {
    let audio = fake.buffered_audio();
    let calls = calls.all();
    assert!(audio.len() >= 50, "{} packets", audio.len());
    let anchor = accepted(fake);
    let first = &audio[0].1;
    let skipped = first
        .timestamp
        .wrapping_sub(integer(&anchor, "rtpTime") as u32);
    assert_eq!(skipped % FRAMES_PER_PACKET as u32, 0);
    for (i, (_, packet)) in audio.iter().enumerate() {
        assert_eq!(call_of(packet), i);
        assert_eq!(packet.ssrc, 0x1200_0000);
        assert_eq!(packet.seq, (first.seq + i as u32) % BUFFERED_SEQUENCES);
        assert_eq!(
            packet.timestamp,
            first.timestamp.wrapping_add((i * FRAMES_PER_PACKET) as u32)
        );
        let (called_at, play_at) = calls[i];
        let lead = play_at - called_at;
        assert!(
            lead <= LATENCY && lead > LATENCY - Duration::from_millis(25),
            "call {i} rendered {lead:?} ahead"
        );
        let error = off(plays_at(fake, packet), play_at);
        assert!(error < TIMED_WITHIN, "packet {i} plays {error:?} off");
    }
}

fn teardown_types(fake: &Fake) -> Vec<Option<u64>> {
    requests(fake)
        .iter()
        .filter(|r| r.method == "TEARDOWN")
        .map(|r| {
            r.plist()
                .get("streams")
                .and_then(|s| s.as_array())
                .map(|s| integer(s[0].as_dictionary().unwrap(), "type"))
        })
        .collect()
}

#[test]
fn a_stream_follows_a_leading_receivers_clock_and_each_packet_plays_when_it_said() {
    let Setup {
        fake,
        browser,
        sender,
        receiver,
        settings,
    } = setup(Behavior::default(), LEADING);
    let truth = Timebase::new(
        Timeline {
            clock: receiver.identity(),
            epoch: 0,
        },
        Instant::now(),
        PtpTime {
            nanos: 231_017_000_000_000,
        },
        1.0001,
    )
    .unwrap();
    receiver.set_test_timeline(truth);
    let calls = Calls::default();
    let stream = Stream::start_with(&browser, fake.id(), Ok(settings), calls.renderer());
    assert!(
        wait_until(STARTING, || fake.buffered_audio().len() >= 60),
        "{:?}",
        stream.status()
    );
    let Status::Streaming(counters) = stream.status() else {
        panic!("{:?}", stream.status())
    };
    stream.finish(Duration::ZERO).wait();

    let methods: Vec<String> = requests(&fake).into_iter().map(|r| r.method).collect();
    assert_eq!(
        methods[..8],
        [
            "GET",
            "POST",
            "POST",
            "SETUP",
            "RECORD",
            "SETPEERS",
            "SETUP",
            "SETRATEANCHORTIME"
        ]
    );
    assert_eq!(teardown_types(&fake), [Some(103), None]);
    let session = requests(&fake)[3].plist();
    let peer = session["timingPeerInfo"].as_dictionary().unwrap();
    assert_eq!(integer(peer, "ClockID"), sender.identity().as_u64());
    assert_eq!(
        peer["Addresses"].as_array().unwrap()[0].as_string(),
        Some("127.0.0.1")
    );
    assert_eq!(
        integer(&accepted(&fake), "networkTimeTimelineID"),
        receiver.identity().as_u64(),
        "anchored on the receiver's clock"
    );
    assert_eq!(
        counters.bytes_sent,
        counters.packets_sent * BUFFERED_PACKET_LEN as u64
    );
    check_played(&fake, &calls);
}

#[test]
fn a_stream_leads_a_following_receiver_and_each_packet_plays_when_it_said() {
    let Setup {
        fake,
        browser,
        sender,
        settings,
        ..
    } = setup(Behavior::default(), FOLLOWING);
    let calls = Calls::default();
    let stream = Stream::start_with(&browser, fake.id(), Ok(settings), calls.renderer());
    assert!(
        wait_until(STARTING, || streaming(&stream)
            && fake.buffered_audio().len() >= 60
            && matches!(fake.timing(), Some(Timing::Settled(_)))),
        "{:?}, {:?}",
        stream.status(),
        fake.timing()
    );
    assert_eq!(
        integer(&accepted(&fake), "networkTimeTimelineID"),
        sender.identity().as_u64(),
        "anchored on this host's clock"
    );
    check_played(&fake, &calls);
    stream.finish(Duration::ZERO).wait();
    assert_eq!(teardown_types(&fake), [Some(103), None]);
}

/// Claude: a stream whose anchor the fake answers `delay` late, after
/// turning `refusals` away with 400, renders from the first packet it can
/// still render `LATENCY` ahead, and never one before it.
fn check_start(delay: Duration, refusals: u32) {
    let Setup {
        fake,
        browser,
        settings,
        ..
    } = setup(
        Behavior {
            rtsp: Rtsp::SlowAt(Step::Anchor, delay),
            anchor_refusals: refusals,
            ..Behavior::default()
        },
        FOLLOWING,
    );
    let calls = Calls::default();
    let stream = Stream::start_with(&browser, fake.id(), Ok(settings), calls.renderer());
    assert!(
        wait_until(STARTING, || fake.buffered_audio().len() >= 60
            && matches!(fake.timing(), Some(Timing::Settled(_)))),
        "{delay:?}: {:?}",
        stream.status()
    );
    assert_eq!(fake.anchors().len() as u32, refusals + 1);
    let anchor = accepted(&fake);
    let first = fake.buffered_audio()[0].1.timestamp;
    let skipped = first.wrapping_sub(integer(&anchor, "rtpTime") as u32) as u64;
    let skipped = span(skipped);
    assert!(
        skipped >= delay && skipped < delay + Duration::from_millis(60),
        "{delay:?}: started {skipped:?} after the anchor"
    );
    check_played(&fake, &calls);
    stream.stop().wait();
}

#[test]
fn an_anchor_accepted_late_starts_the_stream_at_the_first_packet_still_due_a_latency_ahead() {
    check_start(Duration::from_millis(100), 0);
}

#[test]
fn anchors_turned_away_then_accepted_late_start_where_the_accepted_one_allows() {
    check_start(Duration::from_millis(100), 2);
}

#[test]
fn an_anchor_accepted_near_or_past_the_latency_still_starts_a_latency_ahead() {
    check_start(LATENCY - Duration::from_millis(20), 0);
}

#[test]
fn an_anchor_accepted_past_the_latency_skips_what_already_played() {
    check_start(LATENCY + Duration::from_millis(200), 0);
}

/// Claude: how timing leaves the timeline a stream was anchored on.
#[derive(Debug, Clone, Copy)]
enum Change {
    /// Claude: the receiver's clock, which the stream follows, steps 50 ms.
    Step,
    /// Claude: another grandmaster takes over the receiver's timeline.
    Grandmaster,
    /// Claude: this host's clock, which the receiver follows, starts a new
    /// epoch.
    Epoch,
}

/// Claude: a stream whose timing leaves its anchored timeline, while playing
/// or while finishing, ends within a bound, sending no further anchor.
fn check_change(change: Change, finishing: bool) {
    let priority = match change {
        Change::Step | Change::Grandmaster => LEADING,
        Change::Epoch => FOLLOWING,
    };
    let Setup {
        fake,
        browser,
        sender,
        receiver,
        settings,
    } = setup(Behavior::default(), priority);
    let calls = Calls::default();
    let stream = Stream::start_with(&browser, fake.id(), Ok(settings), calls.renderer());
    assert!(
        wait_until(STARTING, || fake.buffered_audio().len() >= 30),
        "{change:?}: {:?}",
        stream.status()
    );
    let anchors = fake.anchors().len();
    let playout = Duration::from_secs(10);
    let (stream, finishing) = if finishing {
        (None, Some(stream.finish(playout)))
    } else {
        (Some(stream), None)
    };
    let now = Instant::now();
    match change {
        Change::Step | Change::Grandmaster => {
            let Some(Timing::Settled(own)) = fake.timing() else {
                panic!(
                    "{change:?}: the receiver is not leading: {:?}",
                    fake.timing()
                )
            };
            let reads = own.network(now).unwrap();
            let (clock, reads) = match change {
                Change::Step => (receiver.identity(), reads.after(Duration::from_millis(50))),
                _ => (crate::ptp::ClockIdentity([9; 8]), Some(reads)),
            };
            let timeline = Timeline { clock, epoch: 0 };
            receiver.set_test_timeline(Timebase::new(timeline, now, reads.unwrap(), 1.0).unwrap());
        }
        Change::Epoch => {
            let timeline = Timeline {
                clock: sender.identity(),
                epoch: 1,
            };
            let reads = PtpTime { nanos: 1 << 40 };
            sender.set_test_timeline(Timebase::new(timeline, now, reads, 1.0).unwrap());
        }
    }
    let bound = Duration::from_secs(5);
    if let Some(finishing) = finishing {
        assert!(
            wait_until(bound, || finishing.is_finished()),
            "{change:?}: finishing went on after its timing changed"
        );
        finishing.wait();
    } else {
        let stream = stream.unwrap();
        assert!(
            wait_until(bound, || matches!(stream.status(), Status::Lost(_))),
            "{change:?}: {:?}",
            stream.status()
        );
        let Status::Lost(reason) = stream.status() else {
            unreachable!()
        };
        assert!(
            reason.contains("PTP timing with the receiver was lost")
                || reason.contains("the receiver's clock")
                || reason.contains("the receiver's timeline moved"),
            "{change:?}: {reason}"
        );
    }
    assert!(
        now.elapsed() < bound,
        "{change:?}: ended {:?} after",
        now.elapsed()
    );
    assert_eq!(fake.anchors().len(), anchors, "{change:?}: anchored again");
    assert!(
        wait_until(Duration::from_secs(1), || teardown_types(&fake)
            == [Some(103), None]),
        "{change:?}: {:?}",
        teardown_types(&fake)
    );
}

#[test]
fn a_stream_whose_receivers_clock_steps_ends_while_playing() {
    check_change(Change::Step, false);
}

#[test]
fn a_stream_whose_receivers_clock_steps_ends_while_finishing() {
    check_change(Change::Step, true);
}

#[test]
fn a_stream_whose_receiver_takes_another_grandmaster_ends_while_playing() {
    check_change(Change::Grandmaster, false);
}

#[test]
fn a_stream_whose_receiver_takes_another_grandmaster_ends_while_finishing() {
    check_change(Change::Grandmaster, true);
}

#[test]
fn a_stream_leading_on_a_new_epoch_ends_while_playing() {
    check_change(Change::Epoch, false);
}

#[test]
fn a_stream_leading_on_a_new_epoch_ends_while_finishing() {
    check_change(Change::Epoch, true);
}

/// Claude: a stream leading a fake that reads its data connection as
/// `reading`, with its own latency, once the fake has its first packets.
fn reading(reading: Reading, latency: Duration) -> (Setup, Stream, Calls) {
    let mut setup = setup(
        Behavior {
            reading,
            ..Behavior::default()
        },
        FOLLOWING,
    );
    setup.settings.latency = latency;
    let calls = Calls::default();
    let stream = Stream::start_with(
        &setup.browser,
        setup.fake.id(),
        Ok(setup.settings.clone()),
        calls.renderer(),
    );
    assert!(
        wait_until(STARTING, || streaming(&stream) && !calls.all().is_empty()),
        "{reading:?}: {:?}",
        stream.status()
    );
    (setup, stream, calls)
}

fn lost(stream: &Stream, within: Duration) -> String {
    assert!(
        wait_until(within, || matches!(stream.status(), Status::Lost(_))),
        "{:?}",
        stream.status()
    );
    let Status::Lost(reason) = stream.status() else {
        unreachable!()
    };
    reason
}

#[test]
fn a_receiver_reading_one_byte_at_a_time_gets_every_packet_whole() {
    let (setup, stream, calls) = reading(Reading::ByteAtATime, LATENCY);
    assert!(wait_until(STARTING, || setup.fake.buffered_audio().len()
        >= 60
        && matches!(setup.fake.timing(), Some(Timing::Settled(_)))));
    assert!(streaming(&stream), "{:?}", stream.status());
    check_played(&setup.fake, &calls);
}

#[test]
fn a_receiver_that_stops_reading_loses_the_stream_while_the_session_is_kept() {
    let latency = Duration::from_secs(2);
    let (setup, stream, _) = reading(Reading::Never, latency);
    let started = Instant::now();
    let reason = lost(&stream, Duration::from_secs(20));
    assert!(
        reason.starts_with("the receiver had not taken a packet"),
        "{reason}"
    );
    assert!(
        started.elapsed() >= latency,
        "lost before any packet was due"
    );
    let feedback = setup
        .fake
        .requests()
        .into_iter()
        .filter(|(at, _, r)| r.uri == "/feedback" && *at > started)
        .count();
    assert!(
        feedback >= 1,
        "the session went unkept while the audio stalled"
    );
    assert_eq!(setup.fake.data_bytes(), 0);
}

#[test]
fn a_receiver_pausing_briefly_gets_everything_and_one_pausing_past_the_play_times_loses_it() {
    let brief = Reading::Pausing {
        after: 50_000,
        pause: Duration::from_millis(100),
    };
    let (setup, stream, calls) = reading(brief, LATENCY);
    assert!(wait_until(STARTING, || setup.fake.buffered_audio().len()
        >= 120
        && matches!(setup.fake.timing(), Some(Timing::Settled(_)))));
    assert!(streaming(&stream), "{:?}", stream.status());
    check_played(&setup.fake, &calls);

    let long = Reading::Pausing {
        after: 50_000,
        pause: Duration::from_secs(60),
    };
    let (setup, stream, _) = reading(long, LATENCY);
    let reason = lost(&stream, Duration::from_secs(20));
    assert!(
        reason.starts_with("the receiver had not taken a packet"),
        "{reason}"
    );
    let audio = setup.fake.buffered_audio();
    assert_eq!(
        audio.len(),
        50_000 / BUFFERED_PACKET_LEN,
        "whole packets up to the pause"
    );
    for (i, (_, packet)) in audio.iter().enumerate() {
        assert_eq!(call_of(packet), i);
    }
}

#[test]
fn a_receiver_closing_its_data_connection_inside_a_packet_loses_the_stream() {
    let closing_after = 10 * BUFFERED_PACKET_LEN + 700;
    let (setup, stream, _) = reading(Reading::ClosingAfter(closing_after), LATENCY);
    let reason = lost(&stream, Duration::from_secs(10));
    assert!(
        reason.starts_with("its data connection failed")
            || reason == "the receiver closed its data connection",
        "{reason}"
    );
    assert_eq!(setup.fake.buffered_audio().len(), 10);
}

#[test]
fn a_stream_to_a_receiver_that_stopped_reading_stops_within_its_bound() {
    let (setup, stream, _) = reading(Reading::Never, LATENCY);
    let stopped_at = Instant::now();
    stream.stop().wait();
    let took = stopped_at.elapsed();
    assert!(
        took < setup.settings.teardown_timeout + PROMPT,
        "stopping took {took:?}"
    );
}

#[test]
fn finishing_sends_everything_rendered_and_ends_when_it_has_played_out() {
    let (setup, stream, calls) = reading(Reading::Steady, LATENCY);
    assert!(wait_until(STARTING, || setup.fake.buffered_audio().len() >= 40));
    let delay = Duration::from_millis(200);
    let finished_at = Instant::now();
    stream.finish(delay).wait();
    let returned = Instant::now();
    let calls = calls.all();
    let played_out = calls.last().unwrap().1 + PACKET_DURATION + delay;
    assert!(returned >= played_out, "{:?} early", played_out - returned);
    assert!(
        returned - finished_at <= LATENCY + delay + setup.settings.teardown_timeout + PROMPT,
        "{:?}",
        returned - finished_at
    );
    assert!(wait_until(Duration::from_secs(1), || setup
        .fake
        .buffered_audio()
        .len()
        == calls.len()));
    assert_eq!(teardown_types(&setup.fake), [Some(103), None]);

    let (setup, stream, _) = reading(Reading::Never, LATENCY);
    let finished_at = Instant::now();
    stream.finish(delay).wait();
    assert!(
        finished_at.elapsed() <= LATENCY + delay + setup.settings.teardown_timeout + PROMPT,
        "{:?}",
        finished_at.elapsed()
    );
}

/// Claude: the methods each connection the fake took carried, in order.
fn by_connection(fake: &Fake) -> Vec<Vec<String>> {
    let mut connections: Vec<Vec<String>> = Vec::new();
    for (_, connection, request) in fake.requests() {
        if connections.len() <= connection {
            connections.resize(connection + 1, Vec::new());
        }
        let name = match request.method.as_str() {
            method if request.body.is_empty() => method.to_owned(),
            "SETUP" => match request.plist().get("streams") {
                Some(streams) => format!(
                    "SETUP {}",
                    integer(
                        streams.as_array().unwrap()[0].as_dictionary().unwrap(),
                        "type"
                    )
                ),
                None => format!(
                    "SETUP {}",
                    request.plist()["timingProtocol"].as_string().unwrap()
                ),
            },
            "TEARDOWN" if request.plist().contains_key("streams") => "TEARDOWN stream".into(),
            "TEARDOWN" => "TEARDOWN session".into(),
            "POST" if request.uri == "/feedback" => continue,
            method => method.to_owned(),
        };
        connections[connection].push(name);
    }
    connections
}

const REALTIME_START: [&str; 7] = [
    "GET",
    "POST",
    "POST",
    "SETUP NTP",
    "SETUP 96",
    "RECORD",
    "FLUSH",
];

/// Claude: a stream to a fake taking buffered audio that falls back to
/// realtime on a fresh connection, once the first is released.
fn check_fallback(
    behavior: Behavior,
    priority1: u8,
    settings: impl FnOnce(&mut Settings),
) -> (Setup, Vec<Vec<String>>) {
    let mut setup = setup(behavior, priority1);
    settings(&mut setup.settings);
    let holders = Arc::strong_count(&setup.sender);
    let in_settings = usize::from(matches!(setup.settings.timing, Some(Ok(_))));
    let stream = Stream::start_with(
        &setup.browser,
        setup.fake.id(),
        Ok(setup.settings.clone()),
        Calls::default().renderer(),
    );
    assert!(
        wait_until(STARTING, || streaming(&stream)),
        "{:?}",
        stream.status()
    );
    assert_eq!(
        Arc::strong_count(&setup.sender),
        holders + in_settings,
        "only the stream's settings hold the clock; the buffered attempt let its timing go"
    );
    let connections = by_connection(&setup.fake);
    let last = connections.len() - 1;
    assert_eq!(connections[last][..7], REALTIME_START);
    assert!(
        setup.fake.open_when_accepted(last).is_empty(),
        "the fallback connected before the buffered attempt let go"
    );
    stream.stop().wait();
    (setup, connections)
}

#[test]
fn without_a_clock_a_stream_starts_as_realtime() {
    let (_, connections) = check_fallback(Behavior::default(), LEADING, |settings| {
        settings.timing = Some(Err("PTP's ports are taken".into()));
    });
    assert_eq!(connections.len(), 1, "{connections:?}");
}

#[test]
fn a_refused_ptp_session_setup_falls_back_on_a_fresh_connection() {
    let behavior = Behavior {
        rtsp: Rtsp::RefuseAt(Step::PtpSessionSetup, 501),
        ..Behavior::default()
    };
    let (_, connections) = check_fallback(behavior, LEADING, |_| {});
    assert_eq!(connections[0], ["GET", "POST", "POST", "SETUP PTP"]);
    assert_eq!(connections.len(), 2);
}

#[test]
fn a_refused_buffered_stream_setup_or_setpeers_releases_the_session_then_falls_back() {
    for (step, through) in [
        (Step::SetPeers, "SETPEERS"),
        (Step::BufferedStreamSetup, "SETUP 103"),
    ] {
        let behavior = Behavior {
            rtsp: Rtsp::RefuseAt(step, 501),
            ..Behavior::default()
        };
        let (_, connections) = check_fallback(behavior, LEADING, |_| {});
        let refused_at = connections[0].iter().position(|m| m == through).unwrap();
        assert_eq!(
            connections[0][refused_at + 1..],
            ["TEARDOWN session"],
            "{step:?}"
        );
        assert_eq!(connections.len(), 2);
    }
}

#[test]
fn timing_that_does_not_settle_within_the_startup_budget_falls_back() {
    let budget = Duration::from_secs(1);
    let (setup, connections) = check_fallback(Behavior::default(), FOLLOWING, |settings| {
        settings.startup_budget = budget;
    });
    assert_eq!(
        connections[0],
        [
            "GET",
            "POST",
            "POST",
            "SETUP PTP",
            "RECORD",
            "SETPEERS",
            "TEARDOWN session"
        ]
    );
    let requests = setup.fake.requests();
    let answered = requests[3].0;
    let torn_down = requests[6].0;
    assert!(
        torn_down - answered >= budget && torn_down - answered < budget + PROMPT,
        "{:?}",
        torn_down - answered
    );
}

#[test]
fn anchors_turned_away_until_the_budget_ends_fall_back() {
    let behavior = Behavior {
        anchor_refusals: u32::MAX,
        ..Behavior::default()
    };
    let (setup, connections) = check_fallback(behavior, FOLLOWING, |settings| {
        settings.startup_budget = Duration::from_secs(3);
    });
    let tail = &connections[0][connections[0].len() - 3..];
    assert_eq!(
        tail,
        ["SETRATEANCHORTIME", "TEARDOWN stream", "TEARDOWN session"]
    );
    assert!(
        setup.fake.anchors().len() >= 2,
        "anchors were offered again"
    );
}

#[test]
fn an_anchor_refused_outright_falls_back_at_once() {
    let behavior = Behavior {
        rtsp: Rtsp::RefuseAt(Step::Anchor, 501),
        ..Behavior::default()
    };
    let (_, connections) = check_fallback(behavior, FOLLOWING, |_| {});
    let tail = &connections[0][connections[0].len() - 3..];
    assert_eq!(
        tail,
        ["SETRATEANCHORTIME", "TEARDOWN stream", "TEARDOWN session"]
    );
    let offered = connections[0].iter().filter(|m| *m == "SETRATEANCHORTIME");
    assert_eq!(offered.count(), 1);
}

#[test]
fn a_busy_receiver_refuses_a_buffered_session_without_a_realtime_fallback() {
    let behavior = Behavior {
        rtsp: Rtsp::RefuseAt(Step::PtpSessionSetup, 453),
        ..Behavior::default()
    };
    let Setup {
        fake,
        browser,
        settings,
        ..
    } = setup(behavior, LEADING);
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(settings),
        Calls::default().renderer(),
    );
    assert!(wait_until(STARTING, || matches!(
        stream.status(),
        Status::Unreachable(_)
    )));
    let Status::Unreachable(reason) = stream.status() else {
        unreachable!()
    };
    assert!(reason.contains("busy"), "{reason}");
    assert_eq!(by_connection(&fake), [["GET", "POST", "POST", "SETUP PTP"]]);
}

#[test]
fn an_unanswered_anchor_fails_without_a_fallback_after_releasing_the_session() {
    let behavior = Behavior {
        rtsp: Rtsp::IgnoreAt(Step::Anchor),
        ..Behavior::default()
    };
    let Setup {
        fake,
        browser,
        settings,
        ..
    } = setup(behavior, FOLLOWING);
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(settings),
        Calls::default().renderer(),
    );
    assert!(wait_until(STARTING, || matches!(
        stream.status(),
        Status::Unreachable(_)
    )));
    let Status::Unreachable(reason) = stream.status() else {
        unreachable!()
    };
    assert!(
        reason.contains("no answer to SETRATEANCHORTIME"),
        "{reason}"
    );
    let connections = by_connection(&fake);
    assert_eq!(connections.len(), 1);
    let tail = &connections[0][connections[0].len() - 3..];
    assert_eq!(
        tail,
        ["SETRATEANCHORTIME", "TEARDOWN stream", "TEARDOWN session"]
    );
}

#[test]
fn a_stream_stopped_during_its_fallback_ends_at_once() {
    let behavior = Behavior {
        rtsp: Rtsp::RefuseAt(Step::BufferedStreamSetup, 501),
        ..Behavior::default()
    };
    let Setup {
        fake,
        browser,
        settings,
        ..
    } = setup(behavior, LEADING);
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(settings),
        Calls::default().renderer(),
    );
    assert!(wait_until(STARTING, || fake.accepted() == 2));
    let stopped_at = Instant::now();
    stream.stop().wait();
    assert!(stopped_at.elapsed() < PROMPT, "{:?}", stopped_at.elapsed());
    std::thread::sleep(PROMPT);
    assert_eq!(fake.accepted(), 2, "no further connection after stopping");
}

#[test]
fn the_fake_tells_which_connections_were_still_open_when_it_accepted_another() {
    let fake = Fake::start(Behavior::default());
    let first = std::net::TcpStream::connect(fake.address()).unwrap();
    assert!(wait_until(Duration::from_secs(1), || fake.accepted() == 1));
    let second = std::net::TcpStream::connect(fake.address()).unwrap();
    assert!(wait_until(Duration::from_secs(1), || fake.accepted() == 2));
    drop(first);
    std::thread::sleep(Duration::from_millis(50));
    let _third = std::net::TcpStream::connect(fake.address()).unwrap();
    assert!(wait_until(Duration::from_secs(1), || fake.accepted() == 3));
    assert_eq!(fake.open_when_accepted(1), [0]);
    assert_eq!(fake.open_when_accepted(2), [1]);
    drop(second);
}

#[test]
fn a_password_a_pin_or_a_busy_receiver_at_any_buffered_step_never_gets_realtime_airplay_2() {
    for (step, status) in [
        (Step::PtpSessionSetup, 401),
        (Step::Record, 403),
        (Step::SetPeers, 470),
        (Step::BufferedStreamSetup, 453),
        (Step::Anchor, 453),
    ] {
        for forced in [false, true] {
            let behavior = Behavior {
                rtsp: Rtsp::RefuseAt(step, status),
                ..Behavior::default()
            };
            let Setup {
                fake,
                browser,
                mut settings,
                ..
            } = setup(behavior, FOLLOWING);
            fake.advertise(&browser, SHAIRPORT_SYNC_RAOP);
            if !forced {
                settings.protocol = None;
            }
            let stream = Stream::start_with(
                &browser,
                fake.id(),
                Ok(settings),
                Calls::default().renderer(),
            );
            assert!(
                wait_until(STARTING, || !matches!(stream.status(), Status::Connecting)),
                "{step:?} {status}"
            );
            drop(stream);
            std::thread::sleep(Duration::from_millis(100));
            let connections = by_connection(&fake);
            assert!(
                connections
                    .iter()
                    .all(|c| !c.contains(&"SETUP NTP".to_owned())),
                "{step:?} {status}, forced {forced}: {connections:?}"
            );
            let expected = if forced { 1 } else { 2 };
            assert_eq!(
                connections.len(),
                expected,
                "{step:?} {status}: {connections:?}"
            );
            if !forced {
                assert_eq!(connections[1][0], "OPTIONS", "AirPlay 1, as before");
            }
        }
    }
}
