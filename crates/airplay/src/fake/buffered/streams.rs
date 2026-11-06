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
/// Claude: how late a render may come after it is due, the stream's thread
/// waiting its turn on a machine busy with the other tests.
const SCHEDULING: Duration = Duration::from_millis(25);
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

/// Claude: when a packet must reach the fake, by how the fake reads.
#[derive(Debug, Clone, Copy)]
enum Receipt {
    /// Claude: within this long of its render: the stream writes it at once
    /// to a fake reading as it comes, so this is scheduling and loopback.
    Within(Duration),
    /// Claude: before the fake would play it, for a fake that pauses.
    BeforePlay,
    /// Claude: unchecked, for a fake reading one byte at a time, whose TCP
    /// window opens a byte at a time, so that the sender's kernel holds back
    /// what the stream wrote in time.
    Unchecked,
}

const PROMPTLY: Receipt = Receipt::Within(Duration::from_millis(50));

/// Claude: checks that the fake got the packets the renderer was asked for,
/// in order and whole, each rendered `LATENCY` before the play time the
/// stream gave it and received as `receipt` says, and played by the fake, by
/// its reading of the anchor and of its timing now, at the time the stream
/// gave.
fn check_played(fake: &Fake, calls: &Calls, receipt: Receipt) {
    let audio = fake.buffered_audio();
    let calls = calls.all();
    assert!(audio.len() >= 50, "{} packets", audio.len());
    let anchor = accepted(fake);
    let first = &audio[0].1;
    let skipped = first
        .timestamp
        .wrapping_sub(integer(&anchor, "rtpTime") as u32);
    assert_eq!(skipped % FRAMES_PER_PACKET as u32, 0);
    for (i, (received, packet)) in audio.iter().enumerate() {
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
            lead <= LATENCY && lead > LATENCY - SCHEDULING,
            "call {i} rendered {lead:?} ahead"
        );
        let plays = plays_at(fake, packet);
        let error = off(plays, play_at);
        assert!(error < TIMED_WITHIN, "packet {i} plays {error:?} off");
        let promptly = match receipt {
            Receipt::Within(within) => called_at + within,
            Receipt::BeforePlay => plays,
            Receipt::Unchecked => continue,
        };
        assert!(
            *received <= promptly && *received < plays,
            "packet {i} came {:?} after its render, {:?} before it plays",
            received.saturating_duration_since(called_at),
            plays.saturating_duration_since(*received)
        );
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
    check_played(&fake, &calls, PROMPTLY);
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
    check_played(&fake, &calls, PROMPTLY);
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
    check_played(&fake, &calls, PROMPTLY);
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
    check_played(&setup.fake, &calls, Receipt::Unchecked);
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
    check_played(&setup.fake, &calls, Receipt::BeforePlay);

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
}

/// Claude: the latency of the tests under send pressure, long enough that the
/// connection backs up well before any queued packet is due.
const BACKED_UP: Duration = Duration::from_secs(5);

#[test]
fn a_stream_with_audio_queued_behind_a_receiver_that_stopped_reading_stops_within_its_bound() {
    let led = Led::start(Reading::UntilReleased(20_000), BACKED_UP);
    led.backed_up();
    let stopped_at = Instant::now();
    led.stream.stop().wait();
    let took = stopped_at.elapsed();
    assert!(
        took < led.setup.settings.teardown_timeout + PROMPT,
        "stopping took {took:?}"
    );
}

#[test]
fn finishing_with_audio_queued_delivers_it_once_the_receiver_reads_again() {
    let led = Led::start(Reading::UntilReleased(20_000), BACKED_UP);
    led.backed_up();
    let Led {
        setup,
        stream,
        calls,
        ..
    } = led;
    let finishing = stream.finish(Duration::ZERO);
    std::thread::sleep(Duration::from_millis(500));
    setup.fake.release_reading();
    assert!(wait_until(BACKED_UP + Duration::from_secs(2), || finishing
        .is_finished()));
    let status = finishing.status();
    finishing.wait();
    assert!(
        matches!(status, Status::Streaming(_)),
        "finishing ended {status:?}"
    );
    let rendered = calls.all().len();
    assert!(wait_until(Duration::from_secs(2), || setup
        .fake
        .buffered_audio()
        .len()
        == rendered));
    for (i, (_, packet)) in setup.fake.buffered_audio().iter().enumerate() {
        assert_eq!(call_of(packet), i);
    }
}

#[test]
fn finishing_with_audio_queued_behind_a_receiver_that_never_reads_again_ends_lost_when_it_is_due() {
    let led = Led::start(Reading::UntilReleased(20_000), BACKED_UP);
    led.backed_up();
    let Led { stream, .. } = led;
    let finished_at = Instant::now();
    let finishing = stream.finish(Duration::ZERO);
    assert!(wait_until(BACKED_UP + Duration::from_secs(2), || finishing
        .is_finished()));
    let took = finished_at.elapsed();
    let Status::Lost(reason) = finishing.status() else {
        panic!("{:?}", finishing.status())
    };
    finishing.wait();
    assert!(
        reason.starts_with("the receiver had not taken a packet"),
        "{reason}"
    );
    assert!(took <= BACKED_UP + SCHEDULING, "{took:?}");
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

#[test]
fn a_receiver_taking_no_anchor_until_its_own_timing_settles_gets_the_stream_once_it_has() {
    let Setup {
        fake,
        browser,
        settings,
        ..
    } = setup(
        Behavior {
            anchor_until_timed: true,
            ..Behavior::default()
        },
        FOLLOWING,
    );
    let calls = Calls::default();
    let stream = Stream::start_with(&browser, fake.id(), Ok(settings), calls.renderer());
    assert!(
        wait_until(STARTING, || fake.buffered_audio().len() >= 60),
        "{:?}",
        stream.status()
    );
    assert!(
        fake.anchors().len() >= 2,
        "the stream settled leading before the fake had measured its clock"
    );
    check_played(&fake, &calls, PROMPTLY);
}

/// Claude: this host's clock as a test sets it: reading `network` at `host`
/// on `timeline`.
struct Own {
    sender: Arc<Ptp>,
    timeline: Timeline,
    host: Instant,
    network: PtpTime,
}

impl Own {
    /// Claude: moves the clock on the same timeline so that every play time
    /// comes `later` later, as a refined estimate would, at `rate`.
    fn refine(&self, later: Duration, rate: f64) {
        let network = PtpTime {
            nanos: self.network.nanos - later.as_nanos() as u64,
        };
        let timebase = Timebase::new(self.timeline, self.host, network, rate).unwrap();
        self.sender.set_test_timeline(timebase);
    }

    /// Claude: starts a new epoch of the clock.
    fn step(&self) {
        let timeline = Timeline {
            epoch: self.timeline.epoch + 1,
            ..self.timeline
        };
        let timebase = Timebase::new(timeline, self.host, self.network, 1.0).unwrap();
        self.sender.set_test_timeline(timebase);
    }
}

/// Claude: a stream leading the fake on a clock the test sets.
struct Led {
    setup: Setup,
    stream: Stream,
    calls: Calls,
    own: Own,
}

impl Led {
    fn start(reading: Reading, latency: Duration) -> Led {
        let mut setup = setup(
            Behavior {
                reading,
                ..Behavior::default()
            },
            FOLLOWING,
        );
        setup.settings.latency = latency;
        let own = Own {
            sender: setup.sender.clone(),
            timeline: Timeline {
                clock: setup.sender.identity(),
                epoch: 0,
            },
            host: Instant::now(),
            network: PtpTime { nanos: 1 << 40 },
        };
        own.refine(Duration::ZERO, 1.0);
        let calls = Calls::default();
        let stream = Stream::start_with(
            &setup.browser,
            setup.fake.id(),
            Ok(setup.settings.clone()),
            calls.renderer(),
        );
        assert!(
            wait_until(STARTING, || streaming(&stream) && calls.all().len() >= 20),
            "{:?}",
            stream.status()
        );
        Led {
            setup,
            stream,
            calls,
            own,
        }
    }

    /// Claude: waits until rendered packets queue unsent behind a receiver
    /// that stopped reading, returning how many had been sent.
    fn backed_up(&self) -> u64 {
        let sent = || match self.stream.status() {
            Status::Streaming(counters) => counters.packets_sent,
            status => panic!("{status:?}"),
        };
        assert!(
            wait_until(Duration::from_secs(15), || self.calls.all().len() as u64
                > sent() + 5),
            "no packet queued"
        );
        sent()
    }
}

/// Claude: every render after the first `from` gave a play time one packet
/// after the one before, so none was rendered against another mapping.
fn check_steady(calls: &[(Instant, Instant)], from: usize) {
    for pair in calls[from..].windows(2) {
        let step = pair[1].1 - pair[0].1;
        assert!(
            step.abs_diff(PACKET_DURATION) < Duration::from_millis(1),
            "a render {step:?} after the one before"
        );
    }
}

fn leads(calls: &[(Instant, Instant)]) -> impl Iterator<Item = (usize, Duration)> + '_ {
    calls
        .iter()
        .enumerate()
        .map(|(i, (called_at, play_at))| (i, *play_at - *called_at))
}

#[test]
fn a_refined_mapping_delays_the_next_render_to_its_new_due_time() {
    let led = Led::start(Reading::Steady, LATENCY);
    let anchors = led.setup.fake.anchors().len();
    for step in 1..=5 {
        led.own.refine(Duration::from_millis(20) * step, 1.0);
        std::thread::sleep(Duration::from_millis(150));
    }
    for (i, lead) in leads(&led.calls.all()) {
        assert!(
            lead <= LATENCY && lead > LATENCY - SCHEDULING,
            "call {i} rendered {lead:?} ahead"
        );
    }
    assert_eq!(led.setup.fake.anchors().len(), anchors, "anchored again");
    assert!(streaming(&led.stream));
}

#[test]
fn a_mapping_refined_earlier_is_caught_up_at_once() {
    let led = Led::start(Reading::Steady, LATENCY);
    let earlier = Duration::from_millis(20);
    led.own.refine(Duration::from_millis(100), 1.0);
    std::thread::sleep(Duration::from_millis(200));
    led.own.refine(Duration::from_millis(100) - earlier, 1.0);
    std::thread::sleep(Duration::from_millis(200));
    for (i, lead) in leads(&led.calls.all()) {
        assert!(
            lead <= LATENCY && lead > LATENCY - earlier - SCHEDULING,
            "call {i} rendered {lead:?} ahead"
        );
    }
}

/// Claude: finishing ends when the last packet sent has played and its delay
/// after, as the mapping read when finishing began says: refinements after
/// that, here moving every play time 600 ms later, do not move the end. The
/// stream fixes the end on its first step after `finish`, well within the
/// wait before the first refinement, and the first TEARDOWN marks the end, so
/// its answer's wait does not count.
#[test]
fn finishing_ends_when_it_first_said_however_the_mapping_is_refined_after() {
    let Led {
        setup,
        stream,
        calls,
        own,
    } = Led::start(Reading::Steady, LATENCY);
    let delay = Duration::from_millis(100);
    let finishing = stream.finish(delay);
    std::thread::sleep(Duration::from_millis(100));
    let end = calls.all().last().unwrap().1 + PACKET_DURATION + delay;
    for step in 1..=3 {
        own.refine(Duration::from_millis(200) * step, 1.0);
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(Instant::now() < end, "the refinements came after the end");
    finishing.wait();
    let torn_down = setup
        .fake
        .requests()
        .into_iter()
        .find(|(_, _, request)| request.method == "TEARDOWN")
        .expect("a TEARDOWN")
        .0;
    assert!(torn_down >= end, "torn down {:?} early", end - torn_down);
    assert!(
        torn_down < end + Duration::from_millis(50),
        "torn down {:?} after the end fixed when finishing began",
        torn_down - end
    );
}

/// Claude: a stream with rendered packets queued unsent behind a receiver
/// that stopped reading, whose timing then leaves the anchored timeline,
/// while playing or finishing, ends `Lost` without writing another byte of
/// them once the receiver reads again, rendering nothing more, and anchoring
/// nothing new.
fn check_change_with_audio_queued(finishing: bool) {
    let latency = Duration::from_secs(5);
    let led = Led::start(Reading::UntilReleased(20_000), latency);
    let sent = led.backed_up();
    let anchors = led.setup.fake.anchors().len();
    let Led {
        setup,
        stream,
        calls,
        own,
    } = led;
    let (stream, finishing) = if finishing {
        (None, Some(stream.finish(Duration::from_secs(10))))
    } else {
        (Some(stream), None)
    };
    let rendered = calls.all().len();
    own.step();
    let bound = Duration::from_secs(2);
    let status = match (&stream, &finishing) {
        (Some(stream), _) => {
            assert!(
                wait_until(bound, || !streaming(stream)),
                "{:?}",
                stream.status()
            );
            stream.status()
        }
        (_, Some(finishing)) => {
            assert!(wait_until(bound, || finishing.is_finished()));
            finishing.status()
        }
        _ => unreachable!(),
    };
    let Status::Lost(reason) = status else {
        panic!("{status:?}")
    };
    assert!(reason.contains("stepped (epoch 0 to 1)"), "{reason}");
    setup.fake.release_reading();
    std::thread::sleep(Duration::from_millis(300));
    let audio = setup.fake.buffered_audio();
    assert_eq!(audio.len() as u64, sent, "nothing more went out");
    for (i, (_, packet)) in audio.iter().enumerate() {
        assert_eq!(call_of(packet), i);
    }
    let after = setup.fake.data_bytes() - audio.len() * BUFFERED_PACKET_LEN;
    assert!(
        after < BUFFERED_PACKET_LEN,
        "no packet spliced after a part"
    );
    let calls = calls.all();
    assert!(calls.len() <= rendered + 1, "rendered on after the change");
    check_steady(&calls, 0);
    assert_eq!(setup.fake.anchors().len(), anchors);
    drop((stream, finishing));
}

#[test]
fn timing_changing_with_audio_queued_while_playing_ends_without_sending_it() {
    check_change_with_audio_queued(false);
}

#[test]
fn timing_changing_with_audio_queued_while_finishing_ends_without_sending_it() {
    check_change_with_audio_queued(true);
}

/// Claude: a stream following a receiver whose clock then goes silent ends
/// `Lost` once its timing's holdover runs out, while playing or finishing.
fn check_silent_master(finishing: bool) {
    let Setup {
        fake,
        browser,
        settings,
        ..
    } = setup(Behavior::default(), LEADING);
    let calls = Calls::default();
    let stream = Stream::start_with(&browser, fake.id(), Ok(settings), calls.renderer());
    assert!(
        wait_until(STARTING, || fake.buffered_audio().len() >= 30),
        "{:?}",
        stream.status()
    );
    let holdover = Duration::from_secs(15);
    let silent_at = Instant::now();
    fake.stop_timing();
    let bound = holdover + Duration::from_secs(3);
    let status = if finishing {
        let finishing = stream.finish(Duration::from_secs(30));
        assert!(wait_until(bound, || finishing.is_finished()));
        finishing.status()
    } else {
        assert!(wait_until(bound, || !streaming(&stream)));
        stream.status()
    };
    let lost_after = silent_at.elapsed();
    let Status::Lost(reason) = status else {
        panic!("{status:?}")
    };
    assert!(
        reason.starts_with("PTP timing with the receiver was lost"),
        "{reason}"
    );
    assert!(
        lost_after >= holdover - Duration::from_millis(500),
        "lost {lost_after:?} after the silence, which holdover counts from the last exchange"
    );
}

#[test]
fn a_stream_whose_receivers_clock_falls_silent_is_lost_after_holdover_while_playing() {
    check_silent_master(false);
}

#[test]
fn a_stream_whose_receivers_clock_falls_silent_is_lost_after_holdover_while_finishing() {
    check_silent_master(true);
}

#[test]
fn a_malformed_or_closed_answer_at_a_buffered_step_fails_without_fallback_and_lets_go() {
    for behavior in [
        Rtsp::MalformedAt(Step::Anchor),
        Rtsp::CloseAt(Step::BufferedStreamSetup),
        Rtsp::CloseAt(Step::SetPeers),
    ] {
        let Setup {
            fake,
            browser,
            sender,
            settings,
            ..
        } = setup(
            Behavior {
                rtsp: behavior,
                ..Behavior::default()
            },
            FOLLOWING,
        );
        let stream = Stream::start_with(
            &browser,
            fake.id(),
            Ok(settings),
            Calls::default().renderer(),
        );
        assert!(
            wait_until(STARTING, || matches!(
                stream.status(),
                Status::Unreachable(_)
            )),
            "{behavior:?}: {:?}",
            stream.status()
        );
        assert!(
            wait_until(Duration::from_secs(1), || fake.closed(0).is_some()),
            "{behavior:?}: the connection was kept"
        );
        assert_eq!(fake.accepted(), 1, "{behavior:?}: fell back");
        assert!(
            wait_until(Duration::from_secs(1), || Arc::strong_count(&sender) == 1),
            "{behavior:?}: the timing was kept"
        );
    }
}

#[test]
fn a_play_time_the_timeline_cannot_say_after_streaming_starts_loses_the_stream() {
    let led = Led::start(Reading::Steady, LATENCY);
    let anchors = led.setup.fake.anchors().len();
    led.own.refine(Duration::ZERO, f64::MIN_POSITIVE);
    assert!(wait_until(Duration::from_secs(2), || !streaming(
        &led.stream
    )));
    let Status::Lost(reason) = led.stream.status() else {
        panic!("{:?}", led.stream.status())
    };
    assert!(
        reason.contains("beyond what the receiver's timeline can say"),
        "{reason}"
    );
    assert_eq!(led.setup.fake.anchors().len(), anchors);
}

#[test]
fn a_buffered_stream_sets_the_receiver_volume_once_anchored() {
    let Setup {
        fake,
        browser,
        settings,
        ..
    } = setup(Behavior::default(), LEADING);
    let stream = Stream::start_with(
        &browser,
        fake.id(),
        Ok(settings),
        Calls::default().renderer(),
    );
    stream.set_volume(0.25);
    assert!(wait_until(STARTING, || fake.volume() == -22.5));
    let methods = fake.methods();
    let position = |method: &str| methods.iter().position(|m| m == method).unwrap();
    assert!(
        position("SETRATEANCHORTIME") < position("SET_PARAMETER"),
        "{methods:?}"
    );
    assert!(streaming(&stream));
}
