//! Host and guest on 127.0.0.1 in one process: handshake, messages, watchdog and robustness.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, UdpSocket};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use rewrite_bridge::link::*;

fn cfg(game: &str, control_port: u16) -> LinkConfig {
    LinkConfig {
        control_port,
        heartbeat: Duration::from_millis(50),
        timeout: Duration::from_millis(400),
        reconnect: Duration::from_millis(50),
        ..LinkConfig::new(game)
    }
}

struct Pair {
    host: Link,
    guest: Link,
    he: Vec<Event>,
    ge: Vec<Event>,
}

impl Pair {
    fn new() -> Self {
        let host = Link::host(cfg("Fake Host", 0)).unwrap();
        let port = host.control_addr().unwrap().port();
        let guest = Link::guest(cfg("Fake Guest", port)).unwrap();
        let mut p = Self {
            host,
            guest,
            he: vec![],
            ge: vec![],
        };
        assert!(p.until(|p| p.host.is_connected() && p.guest.is_connected()));
        p
    }

    /// Polls both sides until `f` holds, for at most two seconds.
    fn until(&mut self, f: impl Fn(&Self) -> bool) -> bool {
        self.until_polling(true, f)
    }

    fn until_polling(&mut self, poll_guest: bool, f: impl Fn(&Self) -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(2);
        while Instant::now() < end {
            self.he.extend(self.host.poll());
            if poll_guest {
                self.ge.extend(self.guest.poll());
            }
            if f(self) {
                return true;
            }
            sleep(Duration::from_millis(2));
        }
        false
    }

    fn clear(&mut self) {
        self.he.clear();
        self.ge.clear();
    }
}

fn count<T>(events: &[Event], f: impl Fn(&Event) -> Option<T>) -> usize {
    events.iter().filter_map(f).count()
}

fn pose(entity: u32, time_us: u64, x: f32) -> Pose {
    Pose {
        translation: [x, 0.0, 0.0],
        ..Pose::new(entity, time_us)
    }
}

#[test]
fn handshake_then_every_message() {
    let mut p = Pair::new();
    let hp = p.guest.peer().unwrap().clone();
    let gp = p.host.peer().unwrap().clone();
    assert_eq!(
        (hp.role, hp.game.as_str(), hp.protocol),
        (Role::Host, "Fake Host", 1)
    );
    assert_eq!(
        (gp.role, gp.game.as_str(), gp.protocol),
        (Role::Guest, "Fake Guest", 1)
    );
    assert_eq!(hp.session, gp.session);
    assert_ne!(hp.session, 0);
    assert_eq!(p.host.focus(), Role::Host);
    assert!(matches!(p.he[0], Event::Connected(_)) && matches!(p.ge[0], Event::Connected(_)));
    p.clear();

    // Camera, host to guest.
    let cam = Camera::look_at([0.0, 2.0, 10.0], [0.0; 3], 1.0, 0.1, 500.0, [1280, 720]);
    let cam_seq = p.host.send_camera(&cam).unwrap();
    assert!(p.until(|p| count(&p.ge, |e| matches!(e, Event::Camera { .. }).then_some(())) == 1));
    let Some(Event::Camera { header, camera }) =
        p.ge.iter().find(|e| matches!(e, Event::Camera { .. }))
    else {
        unreachable!()
    };
    assert_eq!(
        (*camera, header.seq, header.session),
        (cam, cam_seq, hp.session)
    );

    // Spawn and 250 poses (three datagrams), guest to host.
    p.guest
        .spawn(&Spawn {
            entity: 1,
            provider: "Warframe".into(),
            asset: "/Lotus/Demo".into(),
            name: None,
        })
        .unwrap();
    let t = now_us();
    let poses: Vec<Pose> = (0..250).map(|i| pose(i, t, i as f32)).collect();
    p.guest.send_poses(&poses).unwrap();
    let got = |p: &Pair| -> Vec<Pose> {
        p.he.iter()
            .flat_map(|e| match e {
                Event::Poses { poses, .. } => poses.clone(),
                _ => vec![],
            })
            .collect()
    };
    assert!(p.until(|p| got(p).len() == 250));
    assert_eq!(got(&p), poses);
    assert!(p.he.contains(&Event::Spawn(Spawn {
        entity: 1,
        provider: "Warframe".into(),
        asset: "/Lotus/Demo".into(),
        name: None,
    })));

    // An older pose of entity 3 is dropped as stale; a newer one passes.
    let stale_before = p.host.stats().stale;
    p.guest
        .send_poses(&[pose(3, t - 1000, 9.0), pose(3, t + 1000, 9.0)])
        .unwrap();
    assert!(p.until(|p| got(p).len() == 251));
    assert_eq!(got(&p)[250].time_us, t + 1000);
    assert_eq!(p.host.stats().stale, stale_before + 1);
    p.clear();

    // Collision of 500 triangles: three chunks, reassembled.
    let tris: Vec<Triangle> = (0..500)
        .map(|i| {
            let x = i as f32;
            [[x, 0.0, 0.0], [x + 1.0, 0.0, 0.0], [x, 0.0, -1.0]]
        })
        .collect();
    let q = p.host.send_collision([1.0, 2.0, 3.0], 30.0, &tris).unwrap();
    assert!(p.until(|p| p.ge.iter().any(|e| matches!(e, Event::Collision(_)))));
    assert_eq!(
        p.ge.iter().find(|e| matches!(e, Event::Collision(_))),
        Some(&Event::Collision(Collision {
            query: q,
            center: [1.0, 2.0, 3.0],
            radius: 30.0,
            triangles: tris,
        }))
    );

    // Focus: the guest asks, the host grants, then takes it back.
    p.guest.set_focus(Role::Guest).unwrap();
    assert_eq!(
        p.guest.focus(),
        Role::Host,
        "a guest's request is not a grant"
    );
    assert!(p.until(|p| p.he.contains(&Event::Focus {
        focus: Role::Guest,
        requested: true
    })));
    p.host.set_focus(Role::Guest).unwrap();
    assert!(p.until(|p| p.guest.focus() == Role::Guest));
    assert!(p.ge.contains(&Event::Focus {
        focus: Role::Guest,
        requested: false
    }));
    p.host.set_focus(Role::Host).unwrap();
    assert!(p.until(|p| p.guest.focus() == Role::Host));

    // A frame through the pixel file, tied to the camera it was rendered with.
    let mut writer = FrameWriter::create_temp("loopback-test", 32, 16, 3).unwrap();
    p.guest.announce_frames(writer.channel()).unwrap();
    assert!(p.until(|p| p.he.iter().any(|e| matches!(e, Event::FrameChannel(_)))));
    let Some(Event::FrameChannel(ch)) = p.he.iter().find(|e| matches!(e, Event::FrameChannel(_)))
    else {
        unreachable!()
    };
    let reader = FrameReader::open(Path::new(&ch.path)).unwrap();
    let pixels: Vec<u8> = (0..32 * 16 * 4).map(|i| (i % 251) as u8).collect();
    let ready = writer.write(&pixels, 32, 16, cam_seq, now_us()).unwrap();
    p.guest.send_frame(&ready).unwrap();
    assert!(p.until(|p| p.he.iter().any(|e| matches!(e, Event::Frame { .. }))));
    let Some(Event::Frame { frame, .. }) = p.he.iter().find(|e| matches!(e, Event::Frame { .. }))
    else {
        unreachable!()
    };
    let mut out = Vec::new();
    let data = reader.read(frame, &mut out).unwrap();
    assert_eq!(out, pixels);
    assert_eq!(data.camera_seq, cam_seq);

    p.guest.despawn(1).unwrap();
    assert!(p.until(|p| p.he.contains(&Event::Despawn { entity: 1 })));

    // Heartbeats flowed both ways and nothing was malformed or foreign.
    let (hs, gs) = (p.host.stats(), p.guest.stats());
    assert!(hs.datagrams_received > 0 && gs.datagrams_received > 0);
    assert_eq!((hs.malformed, gs.malformed, hs.foreign), (0, 0, 0));
}

#[test]
fn watchdog_notices_a_silent_peer_and_both_sides_recover() {
    let mut p = Pair::new();
    let first = p.host.peer().unwrap().session;
    p.clear();
    // The guest freezes: its sockets stay open but it no longer polls, so no heartbeats.
    let frozen = Instant::now();
    assert!(p.until_polling(false, |p| !p.host.is_connected()));
    let after = frozen.elapsed();
    let reason = match &p.he[..] {
        [Event::Lost { reason }] => reason.clone(),
        other => panic!("{other:?}"),
    };
    assert!(reason.starts_with("watchdog"), "{reason}");
    assert!(
        after >= Duration::from_millis(400) && after < Duration::from_millis(900),
        "{after:?}"
    );
    // The guest wakes up, finds the session gone, and both reconnect with a new session.
    assert!(p.until(|p| p.ge.iter().any(|e| matches!(e, Event::Lost { .. }))));
    assert!(p.until(|p| p.host.is_connected() && p.guest.is_connected()));
    assert_ne!(p.host.peer().unwrap().session, first);
}

#[test]
fn a_closed_peer_is_lost_at_once() {
    let host = Link::host(LinkConfig {
        timeout: Duration::from_secs(30),
        ..cfg("Fake Host", 0)
    })
    .unwrap();
    let guest = Link::guest(cfg("Fake Guest", host.control_addr().unwrap().port())).unwrap();
    let mut p = Pair {
        host,
        guest,
        he: vec![],
        ge: vec![],
    };
    assert!(p.until(|p| p.host.is_connected() && p.guest.is_connected()));
    p.clear();
    let t = Instant::now();
    p.guest.disconnect("test over");
    assert!(p.until_polling(false, |p| !p.he.is_empty()));
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "not the 30 s watchdog"
    );
    assert_eq!(
        p.he,
        [Event::Lost {
            reason: "peer said bye: test over".into()
        }]
    );
}

/// Reads one control line from a raw client socket.
fn read_line(r: &mut BufReader<TcpStream>) -> String {
    let mut s = String::new();
    r.read_line(&mut s).unwrap();
    s
}

/// Polls the host on a thread-free loop until `f`, while a raw client does its own I/O.
fn poll_host_until(
    host: &mut Link,
    events: &mut Vec<Event>,
    f: impl Fn(&[Event], &Link) -> bool,
) -> bool {
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        events.extend(host.poll());
        if f(events, host) {
            return true;
        }
        sleep(Duration::from_millis(2));
    }
    false
}

/// A guest written from the contract alone: hand-made JSON and hand-packed datagrams.
#[test]
fn a_foreign_guest_built_from_the_contract() {
    // A long timeout: this guest sends no heartbeats, and the test is about other things.
    let mut host = Link::host(LinkConfig {
        timeout: Duration::from_secs(5),
        ..cfg("Fake Host", 0)
    })
    .unwrap();
    let port = host.control_addr().unwrap().port();
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut reader = BufReader::new(tcp.try_clone().unwrap());
    let mut writer = tcp;
    writeln!(
        writer,
        r#"{{"type":"hello","role":"guest","game":"SKSE plugin","protocol":1,"min_protocol":1,"udp_port":{},"heartbeat_ms":100,"timeout_ms":1000}}"#,
        udp.local_addr().unwrap().port()
    )
    .unwrap();
    let mut events = vec![];
    assert!(poll_host_until(&mut host, &mut events, |_, h| h.is_connected()));
    let reply = read_line(&mut reader);
    let v: serde_json_like::Hello = serde_json_like::parse_hello(&reply);
    assert_eq!(v.role, "host");
    assert_eq!(v.protocol, 1);
    assert_eq!(host.peer().unwrap().game, "SKSE plugin");
    assert_eq!(host.peer().unwrap().session, v.session);

    let host_udp = ("127.0.0.1", v.udp_port);
    let mut dg = Vec::new();
    dg.extend_from_slice(&0x4B4C_4252u32.to_le_bytes());
    dg.extend_from_slice(&1u16.to_le_bytes());
    dg.push(3); // Pose
    dg.push(0);
    dg.extend_from_slice(&v.session.to_le_bytes());
    dg.extend_from_slice(&0u32.to_le_bytes());
    dg.extend_from_slice(&now_us().to_le_bytes());
    dg.extend_from_slice(&1u32.to_le_bytes()); // count
    dg.extend_from_slice(&0u32.to_le_bytes());
    dg.extend_from_slice(&42u32.to_le_bytes()); // entity
    dg.extend_from_slice(&0u32.to_le_bytes()); // flags
    dg.extend_from_slice(&5u64.to_le_bytes()); // time_us
    for f in [
        1.0f32, 2.0, 3.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0,
    ] {
        dg.extend_from_slice(&f.to_le_bytes());
    }
    assert_eq!(dg.len(), 24 + 8 + 80);

    // Garbage from the right address first: counted, never delivered, never a panic.
    let mut rng = 0x1234_5678_9abc_def0u64;
    for i in 0..300usize {
        let mut junk = dg.clone();
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        match i % 3 {
            0 => junk.truncate(rng as usize % dg.len()),
            1 => junk[24 + (rng as usize % 8)] = 0xFF, // count/reserved field: bad count
            _ => {
                junk = (0..rng as usize % 200)
                    .map(|k| (rng >> (k % 56)) as u8)
                    .collect()
            }
        }
        if junk[..junk.len().min(24)] == dg[..junk.len().min(24)] && junk.len() == dg.len() {
            continue; // Only reserved bytes changed: still a valid datagram.
        }
        udp.send_to(&junk, host_udp).unwrap();
    }
    // A stranger on another port, with a valid datagram, is ignored too.
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .send_to(&dg, host_udp)
        .unwrap();
    udp.send_to(&dg, host_udp).unwrap();
    assert!(poll_host_until(&mut host, &mut events, |e, _| e
        .iter()
        .any(|e| matches!(e, Event::Poses { .. }))));
    let poses: Vec<&Pose> = events
        .iter()
        .flat_map(|e| match e {
            Event::Poses { poses, .. } => poses.iter().collect(),
            _ => vec![],
        })
        .collect();
    assert_eq!(poses.len(), 1);
    assert_eq!(poses[0].entity, 42);
    assert_eq!(poses[0].translation, [1.0, 2.0, 3.0]);
    assert_eq!(poses[0].velocity, [0.5, 0.0, 0.0]);
    let s = host.stats();
    assert!(s.malformed >= 150, "{s:?}");
    assert_eq!(s.foreign, 1, "{s:?}");

    // A garbled control line ends the session with a bye that says why.
    writer
        .write_all(b"{\"type\":\"spawn\",\"entity\":\"seven\"}\n")
        .unwrap();
    assert!(poll_host_until(&mut host, &mut events, |_, h| !h.is_connected()));
    let bye = read_line(&mut reader);
    assert!(
        bye.contains("\"bye\"") && bye.contains("protocol error"),
        "{bye}"
    );
}

#[test]
fn version_role_and_busy_rejections() {
    let mut host = Link::host(cfg("Fake Host", 0)).unwrap();
    let port = host.control_addr().unwrap().port();
    let mut events = vec![];
    for (hello, expect) in [
        (
            r#"{"type":"hello","role":"guest","game":"future","protocol":3,"min_protocol":2,"udp_port":9,"heartbeat_ms":100,"timeout_ms":1000}"#,
            "unsupported protocol",
        ),
        (
            r#"{"type":"hello","role":"host","game":"confused","protocol":1,"min_protocol":1,"udp_port":9,"heartbeat_ms":100,"timeout_ms":1000}"#,
            "role host",
        ),
        (
            r#"{"type":"spawn","entity":1,"provider":"p","asset":"a"}"#,
            "expected hello",
        ),
    ] {
        let mut tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        writeln!(&tcp, "{hello}").unwrap();
        events.clear();
        assert!(poll_host_until(&mut host, &mut events, |e, _| !e.is_empty()));
        assert!(
            matches!(&events[..], [Event::HandshakeFailed { reason }] if reason.contains(expect)),
            "{events:?}"
        );
        // The bye, then a clean end of stream (the host half-closes), not a reset.
        let (bye, how) = read_until_closed(&mut host, &mut tcp);
        assert!(bye.contains("\"bye\"") && bye.contains(expect), "{bye}");
        assert!(how.is_ok(), "{how:?}");
        assert!(!host.is_connected());
    }

    // While a guest is connected, a second one is told the host is busy.
    let mut g1 = Link::guest(cfg("first", port)).unwrap();
    let mut g2 = Link::guest(cfg("second", port)).unwrap();
    let mut e2 = vec![];
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end && !(host.is_connected() && g1.is_connected()) {
        host.poll();
        g1.poll();
        sleep(Duration::from_millis(2));
    }
    assert!(g1.is_connected());
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end && e2.is_empty() {
        host.poll();
        g1.poll();
        e2.extend(g2.poll());
        sleep(Duration::from_millis(2));
    }
    assert_eq!(
        e2,
        [Event::HandshakeFailed {
            reason: "peer said bye: busy".into()
        }]
    );
    assert_eq!(host.peer().unwrap().game, "first");
}

#[test]
fn sending_without_a_peer_is_an_error_not_a_panic() {
    let mut guest = Link::guest(cfg("lonely", 1)).unwrap(); // Nothing listens on port 1.
    assert!(guest.poll().is_empty());
    assert!(matches!(
        guest.send_poses(&[Pose::new(1, 0)]),
        Err(LinkError::NotConnected)
    ));
    assert!(matches!(guest.despawn(1), Err(LinkError::NotConnected)));
    assert!(!guest.is_connected());
}

/// Minimal parsing of the host's hello, as a foreign client would do it.
mod serde_json_like {
    pub struct Hello {
        pub role: String,
        pub protocol: u64,
        pub session: u32,
        pub udp_port: u16,
    }

    pub fn parse_hello(line: &str) -> Hello {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["type"], "hello");
        Hello {
            role: v["role"].as_str().unwrap().to_owned(),
            protocol: v["protocol"].as_u64().unwrap(),
            session: v["session"].as_u64().unwrap() as u32,
            udp_port: v["udp_port"].as_u64().unwrap() as u16,
        }
    }
}

/// What a rejected client reads until the host closes: the lines, then how it ended.
fn read_until_closed(host: &mut Link, tcp: &mut TcpStream) -> (String, std::io::Result<()>) {
    use std::io::Read;
    tcp.set_nonblocking(true).unwrap();
    let mut text = Vec::new();
    let mut buf = [0u8; 4096];
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        host.poll();
        match tcp.read(&mut buf) {
            Ok(0) => return (String::from_utf8_lossy(&text).into_owned(), Ok(())),
            Ok(n) => text.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => sleep(Duration::from_millis(2)),
            Err(e) => return (String::from_utf8_lossy(&text).into_owned(), Err(e)),
        }
    }
    panic!(
        "host never closed; got {:?}",
        String::from_utf8_lossy(&text)
    );
}

/// A host that turns a client away while it still has unread data from it must not reset the
/// connection (Windows then discards the bye), but half-close after the bye and drain.
#[test]
fn rejected_clients_get_the_bye_and_a_clean_end() {
    let mut host = Link::host(cfg("Fake Host", 0)).unwrap();
    let port = host.control_addr().unwrap().port();
    let mut first = Link::guest(cfg("first", port)).unwrap();
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end && !(host.is_connected() && first.is_connected()) {
        host.poll();
        first.poll();
        sleep(Duration::from_millis(2));
    }
    assert!(host.is_connected());
    for _ in 0..20 {
        // Busy: the host never reads this client's hello, which is still unread when it closes.
        let mut tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let hello = r#"{"type":"hello","role":"guest","game":"late","protocol":1,"min_protocol":1,"udp_port":9,"heartbeat_ms":100,"timeout_ms":1000}"#;
        writeln!(tcp, "{hello}").unwrap();
        writeln!(tcp, "{}", " ".repeat(30_000)).unwrap();
        sleep(Duration::from_millis(5));
        let (text, how) = read_until_closed(&mut host, &mut tcp);
        assert!(text.contains(r#""reason":"busy""#), "{text:?}");
        assert!(
            how.is_ok(),
            "busy rejection ended with {how:?} after {text:?}"
        );
        first.poll();
    }
}

/// A raw host on a listener of its own, for guest-side tests.
fn raw_host_accepting(guest: &mut Link) -> TcpStream {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut cfg = cfg("Fake Guest", listener.local_addr().unwrap().port());
    cfg.reconnect = Duration::from_secs(10); // One attempt is enough here.
    *guest = Link::guest(cfg).unwrap();
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        guest.poll();
        if let Ok((s, _)) = listener.accept() {
            s.set_nonblocking(false).unwrap();
            return s;
        }
        assert!(Instant::now() < end, "guest never connected");
        sleep(Duration::from_millis(2));
    }
}

fn poll_guest_until(guest: &mut Link, f: impl Fn(&[Event]) -> bool) -> Vec<Event> {
    let mut events = vec![];
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end && !f(&events) {
        events.extend(guest.poll());
        sleep(Duration::from_millis(2));
    }
    events
}

/// A guest that turns a host down says why, then half-closes too.
#[test]
fn a_guest_rejecting_a_host_sends_bye_then_a_clean_end() {
    use std::io::Read;
    let mut guest = Link::guest(cfg("unused", 1)).unwrap();
    let mut tcp = raw_host_accepting(&mut guest);
    // A host hello without a session number is invalid.
    writeln!(
        tcp,
        r#"{{"type":"hello","role":"host","game":"broken","protocol":1,"min_protocol":1,"udp_port":9,"heartbeat_ms":100,"timeout_ms":1000}}"#
    )
    .unwrap();
    let events = poll_guest_until(&mut guest, |e| !e.is_empty());
    assert!(
        matches!(&events[..], [Event::HandshakeFailed { reason }] if reason.contains("session")),
        "{events:?}"
    );
    tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut text = String::new();
    let how = tcp.read_to_string(&mut text);
    assert!(how.is_ok(), "{how:?} after {text:?}");
    let mut lines = text.lines();
    assert!(lines.next().unwrap().contains("\"hello\""));
    let bye = lines.next().unwrap();
    assert!(bye.contains("\"bye\"") && bye.contains("session"), "{bye}");
}

/// A peer that resets the connection right after its bye (a careless implementation, or Windows
/// closing a socket with unread data): the bye still decides how the session ended.
#[test]
fn a_reset_after_a_received_bye_is_a_clean_end() {
    let mut guest = Link::guest(cfg("unused", 1)).unwrap();
    let tcp = raw_host_accepting(&mut guest);
    // Let the guest's hello arrive and leave it unread; then bye and drop, which sends a reset.
    sleep(Duration::from_millis(50));
    writeln!(&tcp, r#"{{"type":"bye","reason":"go away"}}"#).unwrap();
    sleep(Duration::from_millis(20));
    drop(tcp);
    let events = poll_guest_until(&mut guest, |e| !e.is_empty());
    assert_eq!(
        events,
        [Event::HandshakeFailed {
            reason: "peer said bye: go away".into()
        }]
    );
}
