//! Both roles of the link in one thread and one process, driven only by `poll` over loopback: the
//! handshake with and without a token, the endpoint file, poses and a spawn, a camera and a collision
//! snapshot, a guest that reconnects, and a clean drop; and, with a hand-written peer on raw sockets, the
//! rules a library on the other side cannot be trusted to break in the same way (second `hello`, line
//! limits, a peer that stops reading, stale collision chunks, the pause before a retry).
//!
//! These tests run on every target. They exist for `wasm32-wasip2`, a target with no threads, no process
//! ids and no `/dev/urandom`, where a component plugin has to be able to use `Link`; CI runs them there
//! under wasmtime, which gives the component the network and a `/tmp` to write the endpoint file in:
//!
//! ```text
//! export CARGO_TARGET_WASM32_WASIP2_RUNNER="wasmtime run -S inherit-network --dir /tmp::/tmp"
//! cargo test --target wasm32-wasip2 --features link --test link_wasi
//! ```

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::sleep;
use std::time::{Duration, Instant};

use rewrite_bridge::link::*;

/// The tests of this file run one at a time. Several of them need a port that nothing listens on, which they
/// can only get by binding one and closing it; another test binding port 0 in between could be handed the
/// same number. (On `wasm32-wasip2` there is one thread, so this costs nothing.)
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A fresh directory in the temp directory, removed on drop. Its name has a counter and the clock, not
/// the process id. A WASI component has neither a process id nor a temp directory to ask for (`temp_dir`
/// panics there), so it uses the `/tmp` that the runner preopens.
struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        #[cfg(target_os = "wasi")]
        let base = PathBuf::from("/tmp");
        #[cfg(not(target_os = "wasi"))]
        let base = std::env::temp_dir();
        let dir = base.join(format!("rbl-wasi-{}-{tag}-{n}", now_us()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(&self.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn cfg(game: &str, control_port: u16) -> LinkConfig {
    LinkConfig {
        control_port,
        heartbeat: Duration::from_millis(50),
        timeout: Duration::from_millis(2000),
        reconnect: Duration::from_millis(50),
        ..LinkConfig::new(game)
    }
}

/// How long a wait lasts at most. A handshake on loopback takes a few polls; the bound is only there so
/// that a bug fails a test instead of hanging it.
const WAIT: Duration = Duration::from_secs(5);

/// Polls both links until `done` holds on the events seen so far.
fn run_until(
    host: &mut Link,
    guest: &mut Link,
    mut done: impl FnMut(&[Event], &[Event]) -> bool,
) -> (Vec<Event>, Vec<Event>) {
    let (mut eh, mut eg) = (Vec::new(), Vec::new());
    let end = Instant::now() + WAIT;
    while Instant::now() < end && !done(&eh, &eg) {
        eh.extend(host.poll());
        eg.extend(guest.poll());
        sleep(Duration::from_millis(2));
    }
    (eh, eg)
}

/// Polls one link until its session is lost, and returns the reason.
fn wait_lost(link: &mut Link) -> String {
    let end = Instant::now() + WAIT;
    while Instant::now() < end {
        for e in link.poll() {
            if let Event::Lost { reason } = e {
                return reason;
            }
        }
        sleep(Duration::from_millis(2));
    }
    panic!("the session never ended");
}

fn connected(e: &[Event]) -> bool {
    e.iter().any(|e| matches!(e, Event::Connected(_)))
}

fn both_connected(h: &[Event], g: &[Event]) -> bool {
    connected(h) && connected(g)
}

fn failed(e: &[Event]) -> Option<&str> {
    e.iter().find_map(|e| match e {
        Event::HandshakeFailed { reason } => Some(reason.as_str()),
        _ => None,
    })
}

/// The first pose of `entity` among the events.
fn pose_of(e: &[Event], entity: u32) -> Option<&Pose> {
    e.iter().find_map(|e| match e {
        Event::Poses { poses, .. } => poses.iter().find(|p| p.entity == entity),
        _ => None,
    })
}

/// A TCP port nothing listens on: bound to get a number, then closed.
fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

const TOKEN: &str = "5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e";
const WRONG: &str = "5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a90";

#[test]
fn handshake_without_a_token_and_a_clean_drop() {
    let _serial = serial();
    let mut host = Link::host(cfg("Host", 0)).unwrap();
    let port = host.control_addr().unwrap().port();
    let mut guest = Link::guest(cfg("Guest", port)).unwrap();
    let (eh, eg) = run_until(&mut host, &mut guest, both_connected);
    assert!(both_connected(&eh, &eg), "no handshake: {eh:?} {eg:?}");
    let (hp, gp) = (guest.peer().unwrap(), host.peer().unwrap());
    assert_eq!((hp.role, hp.game.as_str()), (Role::Host, "Host"));
    assert_eq!((gp.role, gp.game.as_str()), (Role::Guest, "Guest"));
    assert_eq!(hp.session, gp.session);
    assert_ne!(hp.session, 0);
    let first = hp.session;
    assert_eq!(guest.last_error(), None);
    assert_eq!(host.last_error(), None);

    // Dropping the guest says `bye` and closes the connection gracefully: the host sees a session that
    // ended with the guest's reason, not a broken connection.
    drop(guest);
    let lost = wait_lost(&mut host);
    assert!(lost.contains("closing"), "{lost}");
    assert!(!host.is_connected());

    // A second guest gets a session number of its own: a host that handed out the same one every time
    // would have datagrams of an old session accepted in the new one.
    let mut guest = Link::guest(cfg("Guest", port)).unwrap();
    let (eh, eg) = run_until(&mut host, &mut guest, both_connected);
    assert!(both_connected(&eh, &eg), "{eh:?} {eg:?}");
    let second = host.peer().unwrap().session;
    assert_ne!(second, 0);
    assert_ne!(first, second, "two sessions of one host, one number");
    drop(guest);
    drop(host);
}

#[test]
fn the_right_token_is_accepted_and_the_wrong_one_is_unauthorized() {
    let _serial = serial();
    let mut host = Link::host(LinkConfig {
        token: Some(TOKEN.into()),
        ..cfg("Host", 0)
    })
    .unwrap();
    let port = host.control_addr().unwrap().port();

    let mut right = Link::guest(LinkConfig {
        token: Some(TOKEN.into()),
        ..cfg("Guest", port)
    })
    .unwrap();
    let (eh, eg) = run_until(&mut host, &mut right, both_connected);
    assert!(both_connected(&eh, &eg), "{eh:?} {eg:?}");
    drop(right);
    let lost = wait_lost(&mut host);
    assert!(lost.contains("closing"), "{lost}");

    for (name, token) in [("wrong", Some(WRONG.to_owned())), ("none", None)] {
        let mut guest = Link::guest(LinkConfig {
            token,
            ..cfg("Guest", port)
        })
        .unwrap();
        let (eh, eg) = run_until(&mut host, &mut guest, |h, g| {
            failed(h).is_some() && failed(g).is_some()
        });
        assert!(!connected(&eh) && !connected(&eg), "{name}: connected");
        assert_eq!(failed(&eh), Some(UNAUTHORIZED), "{name}");
        assert!(
            failed(&eg).is_some_and(|r| r.contains(UNAUTHORIZED)),
            "{name}: {eg:?}"
        );
        assert!(!format!("{eh:?}{eg:?}").contains(TOKEN));
    }
}

#[test]
fn the_token_travels_through_the_endpoint_file() {
    let _serial = serial();
    let dir = Dir::new("endpoint");
    let path = dir.path("endpoint.json");
    let mut host = Link::host(LinkConfig {
        endpoint_file: Some(path.clone()),
        ..cfg("Host", 0)
    })
    .unwrap();
    let port = host.control_addr().unwrap().port();

    // The host generated a token (64 lowercase hex digits from the system's random bytes) and wrote it,
    // with its port; no temporary file is left.
    let endpoint = Endpoint::read(&path).unwrap();
    assert_eq!(endpoint.control_port, port);
    assert_eq!(endpoint.token.len(), 64);
    assert!(endpoint
        .token
        .bytes()
        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
    assert_eq!(dir.names(), ["endpoint.json"]);
    // Another host makes another token: a host that wrote one constant token would pass every check
    // above. (Comparing with `generate_token()` would say nothing about what a host does.)
    let other_path = dir.path("other.json");
    let other = Link::host(LinkConfig {
        endpoint_file: Some(other_path.clone()),
        ..cfg("Other", 0)
    })
    .unwrap();
    let other_token = Endpoint::read(&other_path).unwrap().token;
    assert_eq!(other_token.len(), 64);
    assert_ne!(endpoint.token, other_token, "two hosts, one token");
    drop(other);
    assert_eq!(dir.names(), ["endpoint.json"], "a host left its file");
    // WASI has no mode bits: the file is owner-only only where the platform has them.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // A guest that is told only the file connects with that token.
    let mut guest = Link::guest(LinkConfig {
        endpoint_file: Some(path.clone()),
        ..cfg("Guest", 0)
    })
    .unwrap();
    let (eh, eg) = run_until(&mut host, &mut guest, both_connected);
    assert!(both_connected(&eh, &eg), "{eh:?} {eg:?}");
    drop(guest);
    drop(host);
    assert_eq!(
        dir.names(),
        Vec::<String>::new(),
        "the file outlived its host"
    );
}

#[test]
fn poses_and_a_spawn_cross_in_both_directions() {
    let _serial = serial();
    let mut host = Link::host(cfg("Host", 0)).unwrap();
    let port = host.control_addr().unwrap().port();
    let mut guest = Link::guest(cfg("Guest", port)).unwrap();
    let (eh, eg) = run_until(&mut host, &mut guest, both_connected);
    assert!(both_connected(&eh, &eg));

    let spawn = |entity| Spawn {
        entity,
        provider: "Warframe".into(),
        asset: "/Lotus/Demo".into(),
        name: Some("demo".into()),
    };
    let pose = |entity, x| Pose {
        translation: [x, 1.0, 2.0],
        ..Pose::new(entity, now_us())
    };
    // A spawn travels on the control channel and a pose on UDP, so a pose can overtake its spawn and be
    // dropped as the pose of an unknown entity: send the poses once the spawns have arrived.
    guest.spawn(&spawn(7)).unwrap();
    host.spawn(&spawn(9)).unwrap();
    let (eh, eg) = run_until(&mut host, &mut guest, |h, g| {
        h.iter()
            .any(|e| matches!(e, Event::Spawn(s) if s.entity == 7))
            && g.iter()
                .any(|e| matches!(e, Event::Spawn(s) if s.entity == 9))
    });
    assert!(eh
        .iter()
        .any(|e| matches!(e, Event::Spawn(s) if s.entity == 7)));
    assert!(eg
        .iter()
        .any(|e| matches!(e, Event::Spawn(s) if s.entity == 9)));

    guest.send_poses(&[pose(7, 3.0)]).unwrap();
    host.send_poses(&[pose(9, 4.0)]).unwrap();
    let (eh, eg) = run_until(&mut host, &mut guest, |h, g| {
        pose_of(h, 7).is_some() && pose_of(g, 9).is_some()
    });
    assert_eq!(
        pose_of(&eh, 7).map(|p| p.translation),
        Some([3.0, 1.0, 2.0])
    );
    assert_eq!(
        pose_of(&eg, 9).map(|p| p.translation),
        Some([4.0, 1.0, 2.0])
    );
}

/// The biggest datagrams there are: a collision snapshot of 500 triangles is three of them, near the
/// 8192-byte limit.
#[test]
fn a_camera_and_a_collision_snapshot_arrive() {
    let _serial = serial();
    let mut host = Link::host(cfg("Host", 0)).unwrap();
    let port = host.control_addr().unwrap().port();
    let mut guest = Link::guest(cfg("Guest", port)).unwrap();
    let (eh, eg) = run_until(&mut host, &mut guest, both_connected);
    assert!(both_connected(&eh, &eg));

    let camera = Camera::look_at([0.0, 2.0, 10.0], [0.0; 3], 1.0, 0.1, 500.0, [1280, 720]);
    host.send_camera(&camera).unwrap();
    let triangles: Vec<Triangle> = (0..500)
        .map(|i| {
            let x = i as f32;
            [[x, 0.0, 0.0], [x + 1.0, 0.0, 0.0], [x, 0.0, -1.0]]
        })
        .collect();
    let query = host
        .send_collision([1.0, 2.0, 3.0], 30.0, &triangles)
        .unwrap();
    let (_, eg) = run_until(&mut host, &mut guest, |_, g| {
        g.iter().any(|e| matches!(e, Event::Camera { .. }))
            && g.iter().any(|e| matches!(e, Event::Collision(_)))
    });
    assert!(eg
        .iter()
        .any(|e| matches!(e, Event::Camera { camera: c, .. } if *c == camera)));
    assert!(eg.contains(&Event::Collision(Collision {
        query,
        center: [1.0, 2.0, 3.0],
        radius: 30.0,
        triangles,
    })));
}

#[test]
fn a_guest_reconnects_and_says_why_it_could_not_connect() {
    let _serial = serial();
    let port = closed_port();
    let mut guest = Link::guest(cfg("Guest", port)).unwrap();
    assert_eq!(guest.last_error(), None, "nothing has been tried yet");

    // No host: the guest keeps trying and says why the last attempt failed.
    let end = Instant::now() + WAIT;
    while Instant::now() < end && guest.last_error().is_none() {
        guest.poll();
        sleep(Duration::from_millis(2));
    }
    let why = guest.last_error().expect("no reason for the failure");
    assert!(why.contains(&format!("127.0.0.1:{port}")), "{why}");

    // A host appears on that port: the guest connects, and the failure is forgotten.
    let mut host = Link::host(cfg("Host", port)).unwrap();
    let (eh, eg) = run_until(&mut host, &mut guest, both_connected);
    assert!(both_connected(&eh, &eg), "{eh:?} {eg:?}");
    assert_eq!(guest.last_error(), None);

    // The host goes away: the guest sees the session end and, with nobody listening, fails again.
    drop(host);
    let lost = wait_lost(&mut guest);
    assert!(lost.contains("closing"), "{lost}");
    let end = Instant::now() + WAIT;
    while Instant::now() < end && guest.last_error().is_none() {
        guest.poll();
        sleep(Duration::from_millis(2));
    }
    assert!(guest.last_error().is_some());

    // And it reconnects to the next host.
    let mut host = Link::host(cfg("Host", port)).unwrap();
    let (eh, eg) = run_until(&mut host, &mut guest, both_connected);
    assert!(both_connected(&eh, &eg), "{eh:?} {eg:?}");
    assert_eq!(guest.last_error(), None);
}

#[test]
fn a_guest_says_why_it_cannot_use_the_endpoint_file_and_never_shows_the_token() {
    let _serial = serial();
    let dir = Dir::new("guest-file");
    let path = dir.path("endpoint.json");
    let mut guest = Link::guest(LinkConfig {
        endpoint_file: Some(path.clone()),
        ..cfg("Guest", 0)
    })
    .unwrap();
    let end = Instant::now() + WAIT;
    while Instant::now() < end && guest.last_error().is_none() {
        guest.poll();
        sleep(Duration::from_millis(2));
    }
    let why = guest.last_error().expect("no reason for the failure");
    assert!(why.contains("endpoint file"), "{why}");

    // A file that holds a token but is no valid endpoint (port 0) is refused, and the reason names the
    // refusal, not the file's contents.
    fs::write(
        &path,
        format!("{{\"control_port\":0,\"token\":\"{TOKEN}\"}}\n"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let end = Instant::now() + WAIT;
    while Instant::now() < end && !guest.last_error().unwrap_or("").contains("control_port") {
        guest.poll();
        sleep(Duration::from_millis(2));
    }
    let why = guest.last_error().unwrap_or("");
    assert!(why.contains("control_port"), "{why}");
    assert!(!why.contains(TOKEN));

    // A valid file whose host is not listening: the guest has the token by now, and the reason for the
    // failed connection still does not show it.
    let port = closed_port();
    fs::write(
        &path,
        format!("{{\"control_port\":{port},\"token\":\"{TOKEN}\"}}\n"),
    )
    .unwrap();
    let end = Instant::now() + WAIT;
    while Instant::now() < end && !guest.last_error().unwrap_or("").contains("cannot connect") {
        guest.poll();
        sleep(Duration::from_millis(2));
    }
    let why = guest.last_error().unwrap_or("");
    assert!(why.contains(&format!("127.0.0.1:{port}")), "{why}");
    assert!(!why.contains(TOKEN));

    // A host that is up but wants another token: the connection itself works, so the failure is the
    // handshake's, and still no token is shown.
    let mut host = Link::host(LinkConfig {
        token: Some(WRONG.into()),
        ..cfg("Host", 0)
    })
    .unwrap();
    let port = host.control_addr().unwrap().port();
    fs::write(
        &path,
        format!("{{\"control_port\":{port},\"token\":\"{TOKEN}\"}}\n"),
    )
    .unwrap();
    let (_, eg) = run_until(&mut host, &mut guest, |_, g| failed(g).is_some());
    assert!(
        failed(&eg).is_some_and(|r| r.contains(UNAUTHORIZED)),
        "{eg:?}"
    );
    // The connection itself worked, so there is no connection error, and no token in what the guest was told.
    assert_eq!(guest.last_error(), None);
    assert!(!format!("{eg:?}").contains(TOKEN));
}

/// The pixel file needs memory-mapped files, which this target does not have: creating or opening one is
/// an error that says so, and leaves nothing behind.
#[cfg(target_os = "wasi")]
#[test]
fn the_pixel_file_is_an_error_here_not_a_panic() {
    let _serial = serial();
    let dir = Dir::new("pixels");
    let path = dir.path("a.frames");
    let opts = FrameFileOptions::new(4, 4, 2).with_depth();
    let errors = [
        ("create", FrameWriter::create(&path, 4, 4, 2).err()),
        (
            "create_temp",
            FrameWriter::create_temp("wasi", 4, 4, 2).err(),
        ),
        ("create_with", FrameWriter::create_with(&path, &opts).err()),
        ("open", FrameReader::open(&path).err()),
    ];
    for (call, e) in errors {
        let e = e.unwrap_or_else(|| panic!("{call} worked"));
        assert_eq!(e.kind(), std::io::ErrorKind::Unsupported, "{call}: {e}");
        assert!(e.to_string().contains("pixel file"), "{call}: {e}");
    }
    assert_eq!(dir.names(), Vec::<String>::new(), "a file was left behind");
}

// ---------------------------------------------------------------------------------------------
// A peer written by hand on raw sockets, and the rules of the control channel and the datagrams that
// `Link` has to keep whatever the other side does.

/// A guest written by hand: a TCP connection, a UDP socket, and what the host sent so far.
struct Raw {
    tcp: TcpStream,
    udp: UdpSocket,
    session: u32,
    seq: u32,
    inbox: Vec<u8>,
}

fn localhost(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

impl Raw {
    /// Connects to `host` as a guest and completes the handshake, polling `host` meanwhile.
    fn connect(host: &mut Link) -> Self {
        let mut raw = Self::start(host);
        assert!(raw.finish(host, WAIT), "no hello from the host");
        assert!(host.is_connected());
        raw
    }

    /// Connects and sends the `hello`, and does not wait for the host's.
    fn start(host: &Link) -> Self {
        let tcp = TcpStream::connect(localhost(host.control_addr().unwrap().port())).unwrap();
        tcp.set_nonblocking(true).unwrap();
        let udp = UdpSocket::bind(localhost(0)).unwrap();
        let hello = format!(
            "{{\"type\":\"hello\",\"role\":\"guest\",\"game\":\"raw\",\"protocol\":1,\"min_protocol\":1,\
             \"udp_port\":{},\"heartbeat_ms\":250,\"timeout_ms\":2000}}\n",
            udp.local_addr().unwrap().port()
        );
        let mut raw = Self {
            tcp,
            udp,
            session: 0,
            seq: 0,
            inbox: Vec::new(),
        };
        raw.tcp.write_all(hello.as_bytes()).unwrap();
        raw
    }

    /// Polls `host` for up to `within` for the host's `hello`; false if it did not come.
    fn finish(&mut self, host: &mut Link, within: Duration) -> bool {
        let end = Instant::now() + within;
        while Instant::now() < end && self.session == 0 {
            host.poll();
            self.read();
            if let Some(Ok(Control::Hello(h))) = self.lines().first().map(|l| Control::parse(l)) {
                self.session = h.session.unwrap();
                self.udp.connect(localhost(h.udp_port)).unwrap();
            }
            sleep(Duration::from_millis(2));
        }
        self.session != 0
    }

    /// Takes in whatever the host has sent.
    fn read(&mut self) {
        let mut buf = [0u8; 4096];
        loop {
            match self.tcp.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.inbox.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
    }

    fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.inbox)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The reason of the `bye` the host sent, if it did.
    fn bye(&mut self) -> Option<String> {
        self.read();
        self.lines().iter().find_map(|l| match Control::parse(l) {
            Ok(Control::Bye { reason }) => Some(reason),
            _ => None,
        })
    }

    /// Writes all of `bytes` to the host without blocking, polling `host` whenever the socket is full.
    fn send(&mut self, host: &mut Link, bytes: &[u8]) {
        let mut at = 0;
        let end = Instant::now() + WAIT;
        while at < bytes.len() {
            assert!(Instant::now() < end, "the host stopped reading");
            match self.tcp.write(&bytes[at..]) {
                Ok(n) => at += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    host.poll();
                }
                Err(e) => panic!("{e}"),
            }
        }
    }

    fn datagram(&mut self, body: &Datagram) {
        let header = Header {
            version: 1,
            flags: 0,
            session: self.session,
            seq: self.seq,
            time_us: now_us(),
        };
        self.seq += 1;
        let mut out = Vec::new();
        encode_datagram(&header, body, &mut out).unwrap();
        self.udp.send(&out).unwrap();
    }
}

fn raw_hello() -> String {
    "{\"type\":\"hello\",\"role\":\"guest\",\"game\":\"raw\",\"protocol\":1,\"min_protocol\":1,\
     \"udp_port\":9,\"heartbeat_ms\":250,\"timeout_ms\":2000}\n"
        .to_owned()
}

/// Polls `host` until its session is lost and returns the reason, or fails after `WAIT`.
fn lost_reason(host: &mut Link) -> String {
    wait_lost(host)
}

/// A control line of exactly `total` bytes, newline included.
fn line_of(total: usize) -> Vec<u8> {
    let (head, tail) = ("{\"type\":\"x\",\"pad\":\"", "\"}\n");
    let mut line = head.as_bytes().to_vec();
    line.resize(total - tail.len(), b'a');
    line.extend_from_slice(tail.as_bytes());
    assert_eq!(line.len(), total);
    line
}

#[test]
fn a_second_hello_ends_the_session_and_says_so() {
    let _serial = serial();
    let mut host = Link::host(cfg("Host", 0)).unwrap();
    let mut raw = Raw::connect(&mut host);
    raw.send(&mut host, raw_hello().as_bytes());
    let lost = lost_reason(&mut host);
    assert!(lost.contains("second hello"), "{lost}");
    assert!(!host.is_connected());
    // the peer is told, with a `bye` (section 6)
    let end = Instant::now() + WAIT;
    while Instant::now() < end && raw.bye().is_none() {
        host.poll();
        sleep(Duration::from_millis(2));
    }
    assert!(raw.bye().is_some_and(|r| r.contains("second hello")));
}

#[test]
fn a_control_line_may_be_65536_bytes_with_its_newline_and_no_more() {
    let _serial = serial();
    let mut host = Link::host(cfg("Host", 0)).unwrap();
    let mut raw = Raw::connect(&mut host);
    // the longest line there is: taken, and ignored (an unknown type)
    raw.send(&mut host, &line_of(MAX_LINE));
    for _ in 0..50 {
        let events = host.poll();
        assert!(
            !events.iter().any(|e| matches!(e, Event::Lost { .. })),
            "{events:?}"
        );
        sleep(Duration::from_millis(2));
    }
    assert!(host.is_connected());
    // one byte more, with its newline: a protocol error that ends the session
    raw.send(&mut host, &line_of(MAX_LINE + 1));
    // the link's own check names the limit; the parser would have refused the line too, with other words
    let lost = lost_reason(&mut host);
    assert!(lost.contains("control line too long"), "{lost}");
    let end = Instant::now() + WAIT;
    while Instant::now() < end && raw.bye().is_none() {
        host.poll();
        sleep(Duration::from_millis(2));
    }
    assert!(raw
        .bye()
        .is_some_and(|r| r.contains("control line too long")));
}

#[test]
fn a_line_with_no_newline_is_too_long_at_65536_bytes_but_not_at_65535() {
    let _serial = serial();
    let mut host = Link::host(cfg("Host", 0)).unwrap();
    let mut raw = Raw::connect(&mut host);
    // 65535 bytes could still become a line of 65536 with its newline: wait for it
    raw.send(&mut host, &vec![b'a'; MAX_LINE - 1]);
    for _ in 0..50 {
        let events = host.poll();
        assert!(
            !events.iter().any(|e| matches!(e, Event::Lost { .. })),
            "{events:?}"
        );
        sleep(Duration::from_millis(2));
    }
    assert!(host.is_connected());
    // one more and no newline can still make a line of 65536
    raw.send(&mut host, b"a");
    let lost = lost_reason(&mut host);
    assert!(lost.contains("too long"), "{lost}");
}

/// The longest any one call of `poll`, `spawn` or a drop may take while a peer holds its socket open and never
/// reads it. `Link` waits nothing for such a peer except the 200 ms of section 6.1 in a drop; the code without
/// the fix took 5 s on macOS and never returned on Linux, so 2 s is far from both and leaves ten times the
/// 200 ms for a slow CI machine.
const STALLED_CALL_LIMIT: Duration = Duration::from_secs(2);

/// Runs `f`, keeps the longest time any call took in `worst`.
fn timed<T>(worst: &mut Duration, f: impl FnOnce() -> T) -> T {
    let began = Instant::now();
    let out = f();
    *worst = (*worst).max(began.elapsed());
    out
}

/// A host whose peer holds its socket open and never reads it, spawned at until it refuses: the kernel's
/// buffers fill, then the link's own queue passes the 1 MiB of section 7. Returns the peer, the number of
/// spawns taken, and the longest call.
fn stall(host: &mut Link) -> (Raw, usize, Duration) {
    let raw = Raw::connect(host);
    let name = "x".repeat(60_000);
    let (mut sent, mut worst) = (0, Duration::ZERO);
    for entity in 0..1000 {
        let spawn = Spawn {
            entity,
            provider: "p".into(),
            asset: "a".into(),
            name: Some(name.clone()),
        };
        if timed(&mut worst, || host.spawn(&spawn)).is_err() {
            return (raw, sent, worst);
        }
        sent += 1;
    }
    panic!("{sent} spawns of 60 kB went into a socket nobody reads");
}

#[test]
fn a_peer_that_stops_reading_is_lost_once_a_mebibyte_is_waiting_and_no_poll_waits_for_it() {
    let _serial = serial();
    let mut host = Link::host(cfg("Host", 0)).unwrap();
    let (raw, sent, mut worst) = stall(&mut host);
    // more than the 1 MiB that may wait (section 7), and not absurdly more: the rule fires as soon as the
    // queue passes it
    assert!(sent * 60_000 > MAX_PENDING_CONTROL, "{sent} spawns");
    let lost = {
        let end = Instant::now() + WAIT;
        let mut reason = None;
        while reason.is_none() && Instant::now() < end {
            for e in timed(&mut worst, || host.poll()) {
                if let Event::Lost { reason: r } = e {
                    reason = Some(r);
                }
            }
            sleep(Duration::from_millis(2));
        }
        reason.expect("the session never ended")
    };
    assert!(lost.contains("stopped reading"), "{lost}");
    // the closing connection is given its 200 ms and then let go; the peer still has its socket open
    for _ in 0..150 {
        timed(&mut worst, || host.poll());
        sleep(Duration::from_millis(2));
    }
    eprintln!("longest call while the peer held its socket without reading: {worst:?}");
    assert!(worst < STALLED_CALL_LIMIT, "a call took {worst:?}");

    // a guest that comes after is served, once the stalled peer has gone
    drop(raw);
    let mut guest = Link::guest(cfg("Guest", host.control_addr().unwrap().port())).unwrap();
    let (eh, eg) = run_until(&mut host, &mut guest, both_connected);
    assert!(both_connected(&eh, &eg), "{eh:?} {eg:?}");
    let mut dropping = Duration::ZERO;
    timed(&mut dropping, || drop(guest));
    timed(&mut dropping, || drop(host));
    assert!(dropping < STALLED_CALL_LIMIT, "a drop took {dropping:?}");
}

#[test]
fn dropping_a_link_whose_peer_stopped_reading_waits_only_the_linger_time() {
    let _serial = serial();
    // How much goes into a socket nobody reads before the link refuses: found out with a first host...
    let mut first = Link::host(cfg("Host", 0)).unwrap();
    let (raw, sent, _) = stall(&mut first);
    drop(first);
    drop(raw);
    // ...then a second one is filled to a little short of that. Its connection is live, the kernel's buffers
    // and the runtime's are full, and what it still holds is under the 1 MiB that would end the session
    // (four spawns of 60 kB are 240 kB less than the refusal; the capacity moves by less between two runs),
    // so dropping it has to say `bye`, which cannot go out, and give up after the 200 ms of section 6.1.
    let mut host = Link::host(cfg("Host", 0)).unwrap();
    let raw = Raw::connect(&mut host);
    let name = "x".repeat(60_000);
    for entity in 0..sent.saturating_sub(4) {
        let spawn = Spawn {
            entity: entity as u32,
            provider: "p".into(),
            asset: "a".into(),
            name: Some(name.clone()),
        };
        host.spawn(&spawn).unwrap();
    }
    let began = Instant::now();
    drop(host);
    let took = began.elapsed();
    eprintln!("drop while the peer held its socket without reading: {took:?}");
    assert!(took < STALLED_CALL_LIMIT, "the drop took {took:?}");
    drop(raw);
}

/// Polls a scratch host for `ms`, so that parked sockets whose peers have gone are closed by it: the list of
/// parked sockets belongs to the process, and the tests of this file share one.
#[cfg(target_os = "wasi")]
fn reap(ms: u64) {
    let mut host = Link::host(cfg("Reap", 0)).unwrap();
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        host.poll();
        sleep(Duration::from_millis(2));
    }
}

/// Does a guest `Link` still go and connect? It does only while fewer than the limit of sockets are parked,
/// and it shows by reaching a plain listener that never answers.
#[cfg(target_os = "wasi")]
fn guest_connects() -> bool {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut guest = Link::guest(cfg("Probe", port)).unwrap();
    let end = Instant::now() + Duration::from_millis(600);
    while Instant::now() < end {
        guest.poll();
        if listener.accept().is_ok() {
            return true;
        }
        sleep(Duration::from_millis(2));
    }
    false
}

/// Four sessions whose peers stall and stay open leave four sockets parked, which is the limit. Then a fifth
/// connection to a host is not answered, and a guest does not connect, each saying why in `last_error`; when
/// one of the peers lets go, the next `poll` closes its socket and the fifth is answered.
#[cfg(target_os = "wasi")]
#[test]
fn four_stalled_peers_fill_the_limit_and_a_fifth_connection_waits_for_one_to_let_go() {
    let _serial = serial();
    reap(300);
    assert!(guest_connects(), "sockets are parked from before this test");

    let mut host = Link::host(cfg("Host", 0)).unwrap();
    let mut stalled = Vec::new();
    for _ in 0..4 {
        let (raw, _, _) = stall(&mut host);
        stalled.push(raw);
    }

    // The host is at the limit: a guest written by hand is not answered, however long it waits (well within
    // the 2 s a hello may take), and the host says why.
    let mut fifth = Raw::start(&host);
    assert!(
        !fifth.finish(&mut host, Duration::from_millis(600)),
        "a host with four parked sockets answered a fifth guest"
    );
    assert!(!host.is_connected());
    let why = host.last_error().expect("a host at the limit says nothing");
    assert!(why.contains("not accepting"), "{why}");

    // A guest `Link` is at the limit too: it never reaches the listener, and says why.
    assert!(!guest_connects());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut guest = Link::guest(cfg("Guest", listener.local_addr().unwrap().port())).unwrap();
    guest.poll();
    let why = guest
        .last_error()
        .expect("a guest at the limit says nothing");
    assert!(why.contains("not connecting"), "{why}");
    drop(guest);

    // One peer lets go: the next poll closes its socket, the host is under the limit again, answers the
    // fifth guest and drops what it said.
    drop(stalled.pop());
    assert!(
        fifth.finish(&mut host, WAIT),
        "the fifth guest was not answered after a peer let go"
    );
    assert!(host.is_connected());
    host.poll();
    assert_eq!(host.last_error(), None);
    assert!(guest_connects(), "a guest still does not connect");

    // The rest let go, and nothing stays parked for the tests after this one.
    drop(fifth);
    drop(stalled);
    drop(host);
    reap(300);
    assert!(guest_connects());
}

fn chunk(query: u32, index: u16, count: u16, tag: f32) -> Datagram {
    Datagram::Collision(CollisionChunk {
        query,
        chunk_index: index,
        chunk_count: count,
        center: [tag, 0.0, 0.0],
        radius: 4.0,
        triangles: vec![[[tag, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]],
    })
}

#[test]
fn a_collision_chunk_of_an_older_query_is_stale() {
    let _serial = serial();
    let mut host = Link::host(cfg("Host", 0)).unwrap();
    let mut raw = Raw::connect(&mut host);
    let mut seen = Vec::new();
    let settle = |host: &mut Link, seen: &mut Vec<Event>| {
        for _ in 0..40 {
            seen.extend(host.poll());
            sleep(Duration::from_millis(2));
        }
    };
    let queries = |seen: &[Event]| -> Vec<u32> {
        seen.iter()
            .filter_map(|e| match e {
                Event::Collision(c) => Some(c.query),
                _ => None,
            })
            .collect()
    };
    // query 5 completes; an older query (3) that arrives after it is stale, whole
    raw.datagram(&chunk(5, 0, 1, 1.0));
    settle(&mut host, &mut seen);
    assert_eq!(queries(&seen), [5]);
    raw.datagram(&chunk(3, 0, 1, 2.0));
    settle(&mut host, &mut seen);
    assert_eq!(
        queries(&seen),
        [5],
        "a chunk of query 3 completed a snapshot"
    );
    assert_eq!(host.stats().stale, 1);
    // query 7 starts (one of two chunks); a chunk of query 6, older than the assembly, is stale too, and
    // the assembly of 7 goes on to complete
    raw.datagram(&chunk(7, 0, 2, 3.0));
    raw.datagram(&chunk(6, 0, 1, 4.0));
    settle(&mut host, &mut seen);
    assert_eq!(
        queries(&seen),
        [5],
        "a chunk of query 6 completed a snapshot"
    );
    assert_eq!(host.stats().stale, 2);
    raw.datagram(&chunk(7, 1, 2, 3.0));
    settle(&mut host, &mut seen);
    assert_eq!(queries(&seen), [5, 7]);
}

/// How the pause before a retry is checked: a failed attempt, then the thing the guest was missing appears at
/// once, and the guest must not find it before `reconnect` has passed.
const RECONNECT: Duration = Duration::from_millis(400);

/// Polls `guest` until it has an error to show and returns when that poll began.
fn first_failure(guest: &mut Link) -> Instant {
    let end = Instant::now() + WAIT;
    loop {
        let began = Instant::now();
        guest.poll();
        if guest.last_error().is_some() {
            return began;
        }
        assert!(Instant::now() < end, "the guest never failed");
        sleep(Duration::from_millis(2));
    }
}

/// Polls both until the guest is connected and returns how long after `began` the poll that connected it
/// started.
fn connect_after(host: &mut Link, guest: &mut Link, began: Instant) -> Duration {
    let end = Instant::now() + WAIT;
    while Instant::now() < end {
        let poll_began = Instant::now();
        host.poll();
        guest.poll();
        if guest.is_connected() {
            return poll_began - began;
        }
        sleep(Duration::from_millis(2));
    }
    panic!("never connected");
}

/// A guest with no pause connects within a few polls of the host's appearing; one that waits connects after
/// `RECONNECT`, less the time the failed poll and this one take. Half of it is far from both.
fn assert_paused(at: Duration) {
    assert!(
        at >= RECONNECT / 2,
        "connected {at:?} after the failed attempt, with {RECONNECT:?} to wait"
    );
}

#[test]
fn a_guest_waits_the_reconnect_time_after_a_refused_connection() {
    let _serial = serial();
    let port = closed_port();
    let mut guest = Link::guest(LinkConfig {
        reconnect: RECONNECT,
        ..cfg("Guest", port)
    })
    .unwrap();
    let began = first_failure(&mut guest);
    // A host appears at once. A guest that retried at once would find it at its next poll.
    let mut host = Link::host(cfg("Host", port)).unwrap();
    assert_paused(connect_after(&mut host, &mut guest, began));
}

#[test]
fn a_guest_waits_the_reconnect_time_after_an_unreadable_endpoint_file() {
    let _serial = serial();
    let dir = Dir::new("pause");
    let path = dir.path("endpoint.json");
    let mut guest = Link::guest(LinkConfig {
        reconnect: RECONNECT,
        endpoint_file: Some(path.clone()),
        ..cfg("Guest", 0)
    })
    .unwrap();
    let began = first_failure(&mut guest);
    // The host writes its file at once.
    let mut host = Link::host(LinkConfig {
        endpoint_file: Some(path),
        ..cfg("Host", 0)
    })
    .unwrap();
    assert_paused(connect_after(&mut host, &mut guest, began));
}

#[test]
fn a_directory_as_the_endpoint_file_is_one_error_not_two_prefixes() {
    let _serial = serial();
    let dir = Dir::new("dir-endpoint");
    let mut guest = Link::guest(LinkConfig {
        endpoint_file: Some(dir.path("")),
        ..cfg("Guest", 0)
    })
    .unwrap();
    first_failure(&mut guest);
    let why = guest.last_error().unwrap();
    assert_eq!(why, "endpoint file: not a regular file");
}

/// A listener that never accepts, its accept queue full: a connect to it does not complete. A guest with no
/// threads connects inside `poll`, which must not wait for it more than a moment.
#[cfg(target_os = "wasi")]
#[test]
fn a_host_that_never_accepts_holds_a_poll_for_a_moment_only() {
    let _serial = serial();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // The first connect that times out is the one that finds the queue full (the runtime's backlog is a
    // few hundred at most; the cap is only there to end a loop that cannot succeed).
    let mut held = Vec::new();
    while held.len() < 10_000 {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(30)) {
            Ok(s) => held.push(s),
            Err(_) => break,
        }
    }
    assert!(held.len() < 10_000, "its accept queue never filled");

    let mut guest = Link::guest(LinkConfig {
        reconnect: Duration::from_millis(50),
        ..cfg("Guest", addr.port())
    })
    .unwrap();
    for _ in 0..3 {
        let began = Instant::now();
        guest.poll();
        let took = began.elapsed();
        // 100 ms is the wait; a poll that waited for the system's own connect timeout takes seconds
        assert!(took < Duration::from_millis(500), "a poll took {took:?}");
        let why = guest.last_error().expect("no reason for the failure");
        assert!(why.contains("timed out"), "{why}");
        sleep(Duration::from_millis(60));
    }
    drop(held);
}
