//! Both roles of the link in one thread and one process, driven only by `poll` over loopback: the
//! handshake with and without a token, the endpoint file, poses and a spawn, a camera and a collision
//! snapshot, a guest that reconnects, and a clean drop.
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
use std::net::TcpListener;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use rewrite_bridge::link::*;

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
    assert_eq!(guest.last_error(), None);
    assert_eq!(host.last_error(), None);

    // Dropping the guest says `bye` and closes the connection gracefully: the host sees a session that
    // ended with the guest's reason, not a broken connection.
    drop(guest);
    let lost = wait_lost(&mut host);
    assert!(lost.contains("closing"), "{lost}");
    assert!(!host.is_connected());
    drop(host);
}

#[test]
fn the_right_token_is_accepted_and_the_wrong_one_is_unauthorized() {
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
    wait_lost(&mut host);

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
    assert_ne!(
        endpoint.token,
        generate_token().unwrap(),
        "same token twice"
    );
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
    wait_lost(&mut guest);
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
    assert!(!guest.last_error().unwrap_or("").contains(TOKEN));
}

/// The pixel file needs memory-mapped files, which this target does not have: creating or opening one is
/// an error that says so, and leaves nothing behind.
#[cfg(target_os = "wasi")]
#[test]
fn the_pixel_file_is_an_error_here_not_a_panic() {
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
