use super::super::{Ptp, PtpTime, Registry, Timeline, signed_nanos};
use super::*;
use std::sync::Barrier;
use std::thread;

fn config(priority1: u8) -> Config {
    Config {
        event_port: 0,
        general_port: 0,
        priority1,
    }
}
fn address(ipv6: bool) -> SocketAddr {
    if ipv6 { "[::1]:0" } else { "127.0.0.1:0" }
        .parse()
        .unwrap()
}

fn settle(peer: &super::super::Peer) -> Timebase {
    let until = Instant::now() + Duration::from_secs(6);
    loop {
        match peer.timing() {
            Timing::Settled(timebase) => return timebase,
            Timing::Lost(reason) => panic!("timing lost: {reason}"),
            Timing::Acquiring => (),
        }
        assert!(Instant::now() < until, "timing did not settle");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn loopback_lead_follow_ipv4_and_ipv6() {
    for ipv6 in [false, true] {
        let a = Ptp::bind(config(100)).unwrap();
        let b = Ptp::bind(config(250)).unwrap();
        let host = Instant::now();
        let truth = Timebase::new(
            Timeline {
                clock: a.identity(),
                epoch: 0,
            },
            host,
            PtpTime {
                nanos: 231_017_000_000_000,
            },
            1.0001,
        )
        .unwrap();
        a.set_test_timeline(truth);
        let a_peer = a.add_peer(address(ipv6), b.endpoint(address(ipv6)));
        let b_peer = b.add_peer(address(ipv6), a.endpoint(address(ipv6)));
        let lead = settle(&a_peer);
        let follow = settle(&b_peer);
        let now = Instant::now();
        let error = signed_nanos(follow.host(truth.network(now).unwrap()).unwrap(), now);
        eprintln!(
            "loopback ipv6={ipv6}, settling={}ms, offset={}ns, rate={}ppm",
            now.duration_since(host).as_millis(),
            error,
            (follow.rate - truth.rate) * 1e6
        );
        assert_eq!(lead.timeline().clock, a.identity());
        assert_eq!(follow.timeline().clock, a.identity());
        assert!(error.abs() < 1_000_000);
        assert!((follow.rate - truth.rate).abs() < 500e-6);
    }
}

#[test]
fn a_better_master_arriving_after_fifteen_hundred_milliseconds_is_followed() {
    let local = Ptp::bind(config(250)).unwrap();
    let remote = Sockets::bind(&config(100)).unwrap();
    let source = PortIdentity {
        clock: ClockIdentity([2; 8]),
        port: 32768,
    };
    let endpoint = Endpoint {
        event: SocketAddr::new(address(false).ip(), remote.event_port),
        general: SocketAddr::new(address(false).ip(), remote.general_port),
    };
    let local_peer = local.add_peer(address(false), endpoint);
    let start = Instant::now();
    let truth = Timebase::new(
        Timeline {
            clock: source.clock,
            epoch: 0,
        },
        start,
        PtpTime {
            nanos: 100_000_000_000,
        },
        1.0,
    )
    .unwrap();
    let send = |message| {
        remote
            .send(address(false), local.endpoint(address(false)), message)
            .unwrap()
    };
    let mut next_sync = start + Duration::from_millis(1500);
    let mut sequence = 0;
    let mut bytes = [0; 2048];
    loop {
        match local_peer.timing() {
            Timing::Acquiring => (),
            Timing::Settled(timebase) => {
                assert_eq!(timebase.timeline().clock, source.clock);
                eprintln!(
                    "late master settled after {} ms",
                    start.elapsed().as_millis()
                );
                break;
            }
            Timing::Lost(reason) => panic!("late master lost: {reason}"),
        }
        let now = Instant::now();
        assert!(now - start < Duration::from_secs(6));
        if now >= next_sync {
            send(Message::new(
                source,
                sequence,
                -3,
                Body::Announce {
                    origin: Timestamp::ZERO,
                    utc_offset: 37,
                    reserved: 0,
                    dataset: Dataset::local(source.clock, 100),
                    time_source: 0xa0,
                },
            ));
            let sent = send(Message::new(
                source,
                sequence,
                -3,
                Body::Sync(Timestamp::ZERO),
            ));
            let mut follow = Message::new(
                source,
                sequence,
                -3,
                Body::FollowUp(Timestamp::from_nanos(truth.network(sent).unwrap().nanos)),
            );
            follow.tlvs = vec![Tlv::Follow(FollowInfo {
                rate_offset: 0,
                timebase: 0,
                phase_change: [0; 12],
                frequency_change: 0,
            })];
            send(follow);
            next_sync += Duration::from_millis(125);
            sequence += 1;
        }
        while let Ok((length, _)) = remote.sockets[0].recv_from(&mut bytes) {
            let received = Instant::now();
            let request = Message::decode(&bytes[..length]).unwrap();
            assert!(matches!(request.body, Body::DelayReq(_)));
            send(Message::new(
                source,
                request.sequence,
                -3,
                Body::DelayResp {
                    received: Timestamp::from_nanos(truth.network(received).unwrap().nanos),
                    requester: request.source,
                },
            ));
        }
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn no_announce_peer_is_led_and_dropping_last_holder_releases_ports() {
    let a = Ptp::bind(config(250)).unwrap();
    let b = Ptp::bind(config(250)).unwrap();
    let ports = config_ports(&a);
    let peer = a.add_peer(address(false), b.endpoint(address(false)));
    assert_eq!(settle(&peer).timeline().clock, a.identity());
    let weak = Arc::downgrade(&a);
    drop(a);
    assert!(weak.upgrade().is_some());
    drop(peer);
    assert!(weak.upgrade().is_none());
    assert!(Ptp::bind(ports).is_ok());
}

fn config_ports(clock: &Ptp) -> Config {
    Config {
        event_port: clock.event_port,
        general_port: clock.general_port,
        priority1: 250,
    }
}

#[test]
fn singleton_concurrent_acquisition_release_and_reacquisition() {
    static REGISTRY: Registry = Registry::new();
    let clock = REGISTRY.acquire(config(250)).unwrap();
    let ports = config_ports(&clock);
    let barrier = Arc::new(Barrier::new(9));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let barrier = barrier.clone();
            let ports = ports.clone();
            thread::spawn(move || {
                let a = REGISTRY.acquire(ports.clone()).unwrap();
                let id = a.identity();
                barrier.wait();
                barrier.wait();
                drop(a);
                for _ in 0..20 {
                    drop(REGISTRY.acquire(ports.clone()).unwrap());
                }
                id
            })
        })
        .collect();
    barrier.wait();
    let identity = clock.identity();
    drop(clock);
    barrier.wait();
    for worker in workers {
        assert_eq!(worker.join().unwrap(), identity);
    }
    assert!(REGISTRY.clock.lock().unwrap().is_none());
    drop(REGISTRY.acquire(ports.clone()).unwrap());
    assert!(Ptp::bind(ports).is_ok());
}

#[test]
fn a_failed_partial_bind_rolls_back_all_sockets() {
    let ports = config_ports(&Ptp::bind(config(250)).unwrap());
    let occupied = bind_socket(true, ports.general_port).unwrap();
    let error = match Ptp::bind(ports.clone()) {
        Ok(_) => panic!("bind succeeded"),
        Err(e) => e,
    };
    assert!(error.contains("could not bind PTP UDP [::]"), "{error}");
    drop(occupied);
    assert!(Ptp::bind(ports).is_ok());
}

#[test]
fn independent_peers_steps_grandmaster_changes_and_silence() {
    let leader = Ptp::bind(config(100)).unwrap();
    let middle = Ptp::bind(config(200)).unwrap();
    let follower = Ptp::bind(config(250)).unwrap();
    let host = Instant::now();
    let truth_a = Timebase::new(
        Timeline {
            clock: leader.identity(),
            epoch: 0,
        },
        host,
        PtpTime {
            nanos: 100_000_000_000,
        },
        1.0001,
    )
    .unwrap();
    let truth_b = Timebase::new(
        Timeline {
            clock: middle.identity(),
            epoch: 0,
        },
        host,
        PtpTime {
            nanos: 900_000_000_000,
        },
        0.9999,
    )
    .unwrap();
    leader.set_test_timeline(truth_a);
    middle.set_test_timeline(truth_b);
    let leader_peer = leader.add_peer(address(false), middle.endpoint(address(false)));
    let follows_a = middle.add_peer(address(false), leader.endpoint(address(false)));
    let leads_b = middle.add_peer(address(false), follower.endpoint(address(false)));
    let follows_b = follower.add_peer(address(false), middle.endpoint(address(false)));
    settle(&follows_a);
    settle(&leads_b);
    settle(&leader_peer);
    let b = settle(&follows_b);
    assert_eq!(b.timeline().clock, middle.identity());
    assert!(
        signed_nanos(
            b.host(truth_b.network(Instant::now()).unwrap()).unwrap(),
            Instant::now()
        )
        .abs()
            < 1_000_000
    );
    let changed = Timebase::new(
        truth_a.timeline(),
        Instant::now(),
        PtpTime {
            nanos: 30_000_000_000,
        },
        1.0001,
    )
    .unwrap();
    leader.set_test_timeline(changed);
    wait_lost(&follows_a);
    drop(follows_a);
    let follows_a = middle.add_peer(address(false), leader.endpoint(address(false)));
    settle(&follows_a);
    let unaffected = settle(&follows_b);
    assert_eq!(unaffected.timeline(), b.timeline());
    assert!(
        signed_nanos(
            unaffected
                .host(truth_b.network(Instant::now()).unwrap())
                .unwrap(),
            Instant::now()
        )
        .abs()
            < 1_000_000
    );
    let grandmaster = ClockIdentity([77; 8]);
    let changed = Timebase::new(
        Timeline {
            clock: grandmaster,
            epoch: 0,
        },
        Instant::now(),
        PtpTime {
            nanos: 50_000_000_000,
        },
        0.9999,
    )
    .unwrap();
    leader.set_test_timeline(changed);
    wait_lost(&follows_a);
    drop(follows_a);
    let follows_a = middle.add_peer(address(false), leader.endpoint(address(false)));
    assert_eq!(settle(&follows_a).timeline().clock, grandmaster);
    drop(leader_peer);
    drop(leader);
    thread::sleep(Duration::from_millis(1300));
    assert!(matches!(follows_a.timing(), Timing::Settled(_)));
    let until = Instant::now() + HOLDOVER;
    while !matches!(follows_a.timing(), Timing::Lost(_)) {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(20));
    }
    drop(follows_a);
    assert_eq!(settle(&follows_b).timeline(), b.timeline());
}

struct Fixture {
    sockets: Sockets,
    remote: Sockets,
    relationship: Relationship,
    remote_port: PortIdentity,
    own: Timebase,
}

impl Fixture {
    fn new() -> Self {
        let sockets = Sockets::bind(&config(250)).unwrap();
        let remote = Sockets::bind(&config(100)).unwrap();
        let endpoint = Endpoint {
            event: SocketAddr::new(address(false).ip(), remote.event_port),
            general: SocketAddr::new(address(false).ip(), remote.general_port),
        };
        let now = Instant::now();
        let local = PortIdentity {
            clock: ClockIdentity([1; 8]),
            port: 32777,
        };
        let remote_port = PortIdentity {
            clock: ClockIdentity([2; 8]),
            port: 32768,
        };
        let own = Timebase::new(
            Timeline {
                clock: local.clock,
                epoch: 0,
            },
            now,
            PtpTime {
                nanos: 100_000_000_000,
            },
            1.0,
        )
        .unwrap();
        let mut relationship = Relationship::new(
            endpoint,
            address(false),
            local,
            Dataset::local(local.clock, 250),
            0,
            Arc::new(Mutex::new(Timing::Acquiring)),
            now,
        );
        let announce = Message::new(
            remote_port,
            0,
            0,
            Body::Announce {
                origin: Timestamp::ZERO,
                utc_offset: 37,
                reserved: 0,
                dataset: Dataset::local(remote_port.clock, 100),
                time_source: 0xa0,
            },
        );
        relationship.receive(&announce, now, &sockets, own);
        Self {
            sockets,
            remote,
            relationship,
            remote_port,
            own,
        }
    }

    fn follower(&self) -> &Follower {
        match &self.relationship.role {
            Role::Following(f) => f,
            _ => panic!("not following"),
        }
    }
    fn receive(&mut self, message: &Message, at: Instant) {
        self.relationship
            .receive(message, at, &self.sockets, self.own);
    }

    fn sync(&mut self, sequence: u16) -> Instant {
        let message = Message::new(self.remote_port, sequence, -3, Body::Sync(Timestamp::ZERO));
        let t2 = Instant::now();
        self.receive(&message, t2);
        t2
    }

    fn follow(&self, sequence: u16, t2: Instant) -> Message {
        let t1 = self.own.network(t2).unwrap().nanos - 100_000;
        let mut follow = Message::new(
            self.remote_port,
            sequence,
            -3,
            Body::FollowUp(Timestamp::from_nanos(t1)),
        );
        follow.tlvs = vec![Tlv::Follow(FollowInfo {
            rate_offset: 0,
            timebase: 2,
            phase_change: [0; 12],
            frequency_change: 0,
        })];
        follow
    }

    fn response(&self, sync_sequence: u16) -> Message {
        let (sequence, t3) = self.follower().pending[&sync_sequence].request.unwrap();
        Message::new(
            self.remote_port,
            sequence,
            -3,
            Body::DelayResp {
                received: Timestamp::from_nanos(self.own.network(t3).unwrap().nanos + 100_000),
                requester: self.relationship.source,
            },
        )
    }
}

#[test]
fn associations_reject_wrong_ports_duplicates_stale_replies_and_wrap() {
    let mut f = Fixture::new();
    if let Role::Following(follower) = &mut f.relationship.role {
        follower.delay_sequence = 65533;
    }
    for seq in [65534_u16, 65535, 0, 1] {
        let t2 = f.sync(seq);
        let request = f.follower().pending[&seq].request.unwrap();
        let duplicate = Message::new(f.remote_port, seq, -3, Body::Sync(Timestamp::ZERO));
        f.receive(&duplicate, Instant::now());
        assert_eq!(f.follower().pending[&seq].request, Some(request));
        let follow = f.follow(seq, t2);
        let response = f.response(seq);
        let mut wrong_source = follow.clone();
        wrong_source.source.port += 1;
        f.receive(&wrong_source, Instant::now());
        assert!(f.follower().pending[&seq].follow.is_none());
        let mut wrong_requester = response.clone();
        if let Body::DelayResp { requester, .. } = &mut wrong_requester.body {
            requester.port += 1;
        }
        f.receive(&wrong_requester, Instant::now());
        assert!(f.follower().pending[&seq].response.is_none());
        let mut wrong_sequence = response.clone();
        wrong_sequence.sequence = wrong_sequence.sequence.wrapping_add(1);
        f.receive(&wrong_sequence, Instant::now());
        assert!(f.follower().pending[&seq].response.is_none());
        f.receive(&response, Instant::now());
        assert!(f.follower().pending[&seq].response.is_some());
        f.receive(&follow, Instant::now());
        assert!(!f.follower().pending.contains_key(&seq));
        let completed = f.follower().last_sample.unwrap();
        assert_eq!(completed, t2 + request.1.duration_since(t2) / 2);
        f.receive(&follow, Instant::now());
        f.receive(&response, Instant::now());
        f.receive(&duplicate, Instant::now());
        assert!(f.follower().pending.is_empty());
        assert_eq!(f.follower().last_sample, Some(completed));
    }
    let stale = Message::new(f.remote_port, 65500, -3, Body::Sync(Timestamp::ZERO));
    f.receive(&stale, Instant::now());
    assert!(f.follower().pending.is_empty());
    let seq = 2;
    let t2 = f.sync(seq);
    let follow = f.follow(seq, t2);
    let response = f.response(seq);
    let late = Instant::now() + EXCHANGE_LIFETIME;
    f.receive(&response, late);
    f.receive(&follow, late);
    assert!(f.follower().pending[&seq].sync.is_none());
}

#[test]
fn follow_up_before_sync_and_late_complete_exchange_do_not_rewind_epoch() {
    let mut f = Fixture::new();
    let t2 = Instant::now();
    let follow = f.follow(1, t2);
    f.receive(&follow, t2);
    f.sync(1);
    let response1 = f.response(1);
    let t2 = f.sync(2);
    let mut follow = f.follow(2, t2);
    if let Tlv::Follow(info) = &mut follow.tlvs[0] {
        info.timebase = 3;
    }
    f.receive(&follow, Instant::now());
    let response2 = f.response(2);
    f.receive(&response2, Instant::now());
    let completed = f.follower().last_sample;
    let timeline = f.follower().servo.timeline();
    f.receive(&response1, Instant::now());
    assert_eq!(f.follower().last_sample, completed);
    assert_eq!(f.follower().servo.timeline(), timeline);
    assert!(f.follower().pending.is_empty());
}

fn drain(socket: &UdpSocket) -> Vec<(Message, SocketAddr, Instant)> {
    let mut messages = Vec::new();
    let mut bytes = [0; 2048];
    loop {
        let mut descriptor = libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut descriptor, 1, 20) } != 1 {
            break;
        }
        let (length, source) = socket.recv_from(&mut bytes).unwrap();
        let at = Instant::now();
        messages.push((Message::decode(&bytes[..length]).unwrap(), source, at));
    }
    messages
}

#[test]
fn leadership_sends_announce_before_follow_up_on_the_right_ports_and_honors_intervals() {
    let mut f = Fixture::new();
    let now = f.relationship.election_until;
    f.relationship.lead(now, f.own);
    f.relationship.tick(now, &f.sockets, f.own);
    let event = drain(&f.remote.sockets[0]);
    let general = drain(&f.remote.sockets[1]);
    assert_eq!(event.len(), 1);
    assert!(matches!(event[0].0.body, Body::Sync(_)));
    assert_eq!(event[0].1.port(), f.sockets.event_port);
    assert_eq!(general.len(), 3);
    assert!(matches!(general[0].0.body, Body::Announce { .. }));
    assert!(matches!(general[1].0.body, Body::Signaling { .. }));
    assert!(matches!(general[2].0.body, Body::FollowUp(_)));
    assert_eq!(general[2].0.sequence, event[0].0.sequence);
    for (_, source, _) in &general {
        assert_eq!(source.port(), f.sockets.general_port);
    }
    let mut request = Message::new(
        f.remote_port,
        0,
        -128,
        Body::Signaling {
            target: f.relationship.source,
        },
    );
    request.tlvs = vec![Tlv::Interval {
        link: 0,
        sync: -4,
        announce: 127,
        flags: 3,
        reserved: 0,
    }];
    f.receive(&request, now);
    assert_eq!(
        f.relationship.sync_interval,
        Some(Duration::from_micros(62_500))
    );
    assert_eq!(f.relationship.announce_interval, None);
    f.relationship.tick(now, &f.sockets, f.own);
    assert_eq!(drain(&f.remote.sockets[0]).len(), 1);
    assert_eq!(drain(&f.remote.sockets[1]).len(), 1);
    request.tlvs = vec![Tlv::Interval {
        link: 0,
        sync: 126,
        announce: 126,
        flags: 3,
        reserved: 0,
    }];
    f.receive(&request, now);
    assert_eq!(
        f.relationship.sync_interval,
        Some(Duration::from_millis(125))
    );
    assert_eq!(
        f.relationship.announce_interval,
        Some(Duration::from_millis(250))
    );
    let mut delay = Message::new(f.remote_port, u16::MAX, -3, Body::DelayReq(Timestamp::ZERO));
    delay.correction = 123 * 65536;
    f.receive(&delay, now);
    let response = drain(&f.remote.sockets[1]);
    assert_eq!(response.len(), 1);
    assert_eq!(response[0].0.sequence, u16::MAX);
    assert_eq!(response[0].0.correction, delay.correction);
    assert_eq!(
        response[0].0.body,
        Body::DelayResp {
            received: Timestamp::from_nanos(f.own.network(now).unwrap().nanos),
            requester: f.remote_port
        }
    );
}

#[test]
fn software_timestamp_loopback_latency_distribution() {
    const COUNT: usize = 256;
    let sender = Sockets::bind(&config(250)).unwrap();
    let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
    receiver
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let destination = receiver.local_addr().unwrap();
    let (measure, measured) = mpsc::channel();
    let worker = thread::spawn(move || {
        for _ in 0..COUNT {
            let mut bytes = [0; 2048];
            let mut descriptor = libc::pollfd {
                fd: receiver.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 1000) }, 1);
            receiver.recv_from(&mut bytes).unwrap();
            let at = Instant::now();
            measure.send(at).unwrap();
        }
    });
    let endpoint = Endpoint {
        event: destination,
        general: destination,
    };
    let source = PortIdentity {
        clock: ClockIdentity([1; 8]),
        port: 32777,
    };
    let mut samples = Vec::new();
    for sequence in 0..COUNT {
        let sent = sender
            .send(
                address(false),
                endpoint,
                Message::new(source, sequence as u16, -3, Body::Sync(Timestamp::ZERO)),
            )
            .unwrap();
        let received = measured.recv_timeout(Duration::from_secs(1)).unwrap();
        samples.push(received.duration_since(sent).as_nanos());
        thread::sleep(Duration::from_millis(1));
    }
    worker.join().unwrap();
    samples.sort();
    eprintln!(
        "software send-to-recv loopback ns: n={COUNT} min={} median={} p95={} p99={} max={} p99-minus-min={}",
        samples[0],
        samples[COUNT / 2],
        samples[COUNT * 95 / 100],
        samples[COUNT * 99 / 100],
        samples[COUNT - 1],
        samples[COUNT * 99 / 100] - samples[0]
    );
    assert!(samples[COUNT / 2] < 5_000_000);
}

#[test]
fn duplicate_subscribers_and_ipv6_scope_validation() {
    let a = Ptp::bind(config(250)).unwrap();
    let b = Ptp::bind(config(100)).unwrap();
    let peer = a.add_peer(address(false), b.endpoint(address(false)));
    let duplicate = a.add_peer(address(false), b.endpoint(address(false)));
    let remote = b.add_peer(address(false), a.endpoint(address(false)));
    settle(&peer);
    settle(&duplicate);
    settle(&remote);
    drop(peer);
    assert!(matches!(duplicate.timing(), Timing::Settled(_)));
    let invalid = Endpoint {
        event: "[fe80::1%1]:40000".parse().unwrap(),
        general: "[fe80::1%2]:40001".parse().unwrap(),
    };
    assert!(
        route(address(true), invalid)
            .unwrap_err()
            .contains("scopes")
    );
}

#[test]
fn a_second_local_address_is_the_source_of_event_and_general_datagrams() {
    let mut addresses = std::ptr::null_mut();
    assert_eq!(unsafe { libc::getifaddrs(&mut addresses) }, 0);
    let mut cursor = addresses;
    let mut local = None;
    while !cursor.is_null() {
        let interface = unsafe { &*cursor };
        if !interface.ifa_addr.is_null()
            && unsafe { (*interface.ifa_addr).sa_family } == libc::AF_INET as _
        {
            let address = unsafe { &*interface.ifa_addr.cast::<libc::sockaddr_in>() };
            let ip = Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes());
            if ip.is_loopback() && ip != Ipv4Addr::LOCALHOST {
                local = Some(SocketAddr::from((ip, 0)));
            }
        }
        cursor = interface.ifa_next;
    }
    unsafe { libc::freeifaddrs(addresses) };
    let Some(local) = local else {
        eprintln!("second-address PTP test skipped: no second IPv4 loopback address");
        return;
    };
    let clock = Ptp::bind(config(250)).unwrap();
    let remote = Sockets::bind(&config(250)).unwrap();
    let endpoint = Endpoint {
        event: SocketAddr::new(address(false).ip(), remote.event_port),
        general: SocketAddr::new(address(false).ip(), remote.general_port),
    };
    let peer = clock.add_peer(local, endpoint);
    settle(&peer);
    let event = drain(&remote.sockets[0]);
    let mut general = drain(&remote.sockets[1]);
    assert!(
        event
            .iter()
            .any(|(m, _, _)| matches!(m.body, Body::Sync(_)))
    );
    assert!(
        general
            .iter()
            .any(|(m, _, _)| matches!(m.body, Body::Announce { .. }))
    );
    assert!(
        general
            .iter()
            .any(|(m, _, _)| matches!(m.body, Body::FollowUp(_)))
    );
    assert!(
        general
            .iter()
            .any(|(m, _, _)| matches!(m.body, Body::Signaling { .. }))
    );
    remote
        .send(
            address(false),
            clock.endpoint(local),
            Message::new(
                PortIdentity {
                    clock: ClockIdentity([2; 8]),
                    port: 32768,
                },
                17,
                -3,
                Body::DelayReq(Timestamp::ZERO),
            ),
        )
        .unwrap();
    general.extend(drain(&remote.sockets[1]));
    assert!(
        general
            .iter()
            .any(|(m, _, _)| matches!(m.body, Body::DelayResp { .. }))
    );
    for (packets, port) in [(&event, clock.event_port), (&general, clock.general_port)] {
        for (_, source, _) in packets {
            assert_eq!(source.ip(), local.ip());
            assert_eq!(source.port(), port);
        }
    }
    eprintln!("second-address PTP source verified: {}", local.ip());
}

#[test]
fn path_identity_is_stable_and_distinct_for_each_address_family() {
    let clock = Ptp::bind(config(250)).unwrap();
    let remote = Sockets::bind(&config(250)).unwrap();
    let endpoint = |ipv6| Endpoint {
        event: SocketAddr::new(address(ipv6).ip(), remote.event_port),
        general: SocketAddr::new(address(ipv6).ip(), remote.general_port),
    };
    let ipv4 = clock.add_peer(address(false), endpoint(false));
    let ipv6 = clock.add_peer(address(true), endpoint(true));
    for (index, port) in [(1, 32777), (3, 32778)] {
        let packets = drain(&remote.sockets[index]);
        assert!(packets.len() >= 2);
        for (packet, source, _) in packets {
            assert_eq!(
                packet.source,
                PortIdentity {
                    clock: clock.identity(),
                    port
                }
            );
            assert_eq!(source.port(), clock.general_port);
            assert_eq!(source.ip(), address(index == 3).ip());
        }
    }
    drop(ipv4);
    drop(ipv6);
    let _ipv4 = clock.add_peer(address(false), endpoint(false));
    let _ipv6 = clock.add_peer(address(true), endpoint(true));
    for (index, port) in [(1, 32777), (3, 32778)] {
        let packets = drain(&remote.sockets[index]);
        assert!(packets.len() >= 2);
        for (packet, _, _) in packets {
            assert_eq!(packet.source.port, port);
        }
    }
}

#[test]
fn stopping_announces_during_election_leaves_no_send_deadline() {
    let mut f = Fixture::new();
    let now = Instant::now();
    f.relationship.role = Role::Electing;
    f.relationship.election_until = now + ELECTION;
    let mut request = Message::new(
        f.remote_port,
        0,
        -128,
        Body::Signaling {
            target: f.relationship.source,
        },
    );
    request.tlvs = vec![Tlv::Interval {
        link: 0,
        sync: 127,
        announce: 127,
        flags: 3,
        reserved: 0,
    }];
    f.receive(&request, now);
    f.relationship.tick(now, &f.sockets, f.own);
    assert_eq!(f.relationship.deadline(now), now + ELECTION);
    assert!(drain(&f.remote.sockets[0]).is_empty());
    assert!(drain(&f.remote.sockets[1]).is_empty());
}

fn wait_lost(peer: &super::super::Peer) {
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        match peer.timing() {
            Timing::Lost(reason) => {
                assert!(reason.contains("timeline"));
                return;
            }
            Timing::Acquiring => panic!("settled peer returned to Acquiring"),
            Timing::Settled(_) => (),
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn nqptp_wakeup_announces_elect_us_and_a_silent_claimant_is_not_followed_forever() {
    let mut f = Fixture::new();
    let now = Instant::now();
    let announce = |priority| {
        Message::new(
            f.remote_port,
            1,
            0,
            Body::Announce {
                origin: Timestamp::ZERO,
                utc_offset: 37,
                reserved: 0,
                dataset: Dataset::local(f.remote_port.clock, priority),
                time_source: 0xa0,
            },
        )
    };
    let better = announce(249);
    let worse = announce(251);
    f.receive(&better, now);
    assert_eq!(f.relationship.timing, Timing::Acquiring);
    f.receive(&worse, now + Duration::from_millis(150));
    assert_eq!(f.relationship.timing, Timing::Acquiring);
    assert!(matches!(f.relationship.role, Role::Electing));
    f.relationship.tick(
        f.relationship.election_until - Duration::from_nanos(1),
        &f.sockets,
        f.own,
    );
    assert_eq!(f.relationship.timing, Timing::Acquiring);
    f.relationship
        .tick(f.relationship.election_until, &f.sockets, f.own);
    assert!(matches!(f.relationship.timing, Timing::Settled(_)));
    assert!(matches!(f.relationship.role, Role::Leading));
    let mut f = Fixture::new();
    let start = f.follower().started;
    f.relationship
        .tick(start + ANNOUNCE_WITHOUT_TIMING, &f.sockets, f.own);
    assert!(matches!(f.relationship.timing, Timing::Settled(_)));
    let repeated = Message::new(
        f.remote_port,
        2,
        0,
        Body::Announce {
            origin: Timestamp::ZERO,
            utc_offset: 37,
            reserved: 0,
            dataset: Dataset::local(f.remote_port.clock, 100),
            time_source: 0xa0,
        },
    );
    f.receive(
        &repeated,
        start + ANNOUNCE_WITHOUT_TIMING + Duration::from_secs(1),
    );
    assert!(matches!(f.relationship.role, Role::Leading));
}

#[test]
fn holdover_lasts_fifteen_seconds_and_loss_is_final() {
    let mut f = Fixture::new();
    let now = Instant::now();
    let own = f.own;
    let Role::Following(follower) = &mut f.relationship.role else {
        unreachable!()
    };
    for index in 0..9 {
        let host = now + Duration::from_millis(index * 125);
        follower
            .servo
            .observe(Sample {
                host,
                network: own.network(host).unwrap().nanos,
                round_trip: 100_000.0,
            })
            .unwrap();
    }
    let settled = follower.servo.timebase().unwrap();
    follower.has_sync_pair = true;
    follower.last_valid = now + Duration::from_secs(1);
    let last_valid = follower.last_valid;
    f.relationship.publish(Timing::Settled(settled));
    f.relationship
        .tick(last_valid + Duration::from_secs(1), &f.sockets, own);
    assert_eq!(f.relationship.timing, Timing::Settled(settled));
    f.relationship
        .tick(last_valid + Duration::from_millis(14_999), &f.sockets, own);
    assert_eq!(f.relationship.timing, Timing::Settled(settled));
    f.relationship
        .tick(last_valid + Duration::from_secs(15), &f.sockets, own);
    let lost = f.relationship.timing.clone();
    assert!(matches!(lost, Timing::Lost(_)));
    let sync = Message::new(f.remote_port, 1, -3, Body::Sync(Timestamp::ZERO));
    f.receive(&sync, last_valid + Duration::from_secs(16));
    assert_eq!(f.relationship.timing, lost);
}
