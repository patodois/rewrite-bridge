//! The Rust side of the cross-language interop session of `tests/interop/README.md`: a host or a guest that
//! runs the same scripted session as its twins in link-java, link-c and link-cs (handshake, spawns, focus,
//! five lock-step camera/pose/frame rounds with a depth plane, despawns, bye) and prints one result line.
//!
//! cargo run --release --features link --example interop_peer -- host|guest --port 47810 --label rust

use std::collections::VecDeque;
use std::path::Path;
use std::process::exit;
use std::thread::sleep;
use std::time::{Duration, Instant};

use rewrite_bridge::link::*;
use serde_json::json;

const FRAME_W: u32 = 64;
const FRAME_H: u32 = 48;
const SLOTS: u32 = 3;
const ROUNDS: u32 = 5;
const HOST_ENTITY: u32 = 3;
const GUEST_ENTITY: u32 = 7;
const STEP: Duration = Duration::from_secs(10);
const TOTAL: Duration = Duration::from_secs(30);
const BASE_US: u64 = 1_700_000_000_000_000;

// ---------------------------------------------------------------------------------------------
// The data of the session (tests/interop/README.md, "Data")

fn camera(k: u32) -> Camera {
    let kf = k as f32;
    #[rustfmt::skip]
    let view = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, kf, 0.5, -8.0, 1.0];
    #[rustfmt::skip]
    let proj = [1.5, 0.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 0.0, -1.0, -1.0, 0.0, 0.0, -0.25, 0.0];
    Camera {
        view,
        proj,
        fov_y: 1.0,
        near: 0.25,
        far: if k % 2 == 1 { f32::INFINITY } else { 500.0 },
        viewport: [FRAME_W, FRAME_H],
        depth_range: match k % 3 {
            0 => DepthRange::NegOneToOne,
            1 => DepthRange::ZeroToOne,
            _ => DepthRange::Reversed,
        },
    }
}

fn pose(entity: u32, k: u32) -> Pose {
    let kf = k as f32;
    let guest = entity == GUEST_ENTITY;
    Pose {
        entity,
        flags: if k == 1 { POSE_TELEPORT } else { 0 },
        time_us: BASE_US + 1000 * u64::from(k) + if guest { 500 } else { 0 },
        translation: if guest {
            [-kf, 1.0, 4.0]
        } else {
            [kf, 2.0, -3.0]
        },
        rotation: [0.0, 0.0, 0.0, 1.0],
        scale: [1.0, 1.0, 1.0],
        velocity: [0.5, 0.0, 0.0],
        angular_velocity: [0.0, 0.25, 0.0],
    }
}

fn triangles() -> Vec<Triangle> {
    (0..300)
        .map(|i| {
            let b = (i % 64) as f32;
            [[b, 0.0, 0.0], [b + 1.0, 0.0, 0.0], [b, 0.0, 1.0]]
        })
        .collect()
}

fn rgba(k: u32) -> Vec<u8> {
    let mut v: Vec<u8> = (0..FRAME_W * FRAME_H * 4)
        .map(|i| ((37 * i + 11 * k + 5) % 256) as u8)
        .collect();
    v[..4].fill(0);
    v
}

fn depth(k: u32) -> Vec<f32> {
    (0..FRAME_W * FRAME_H)
        .map(|p| {
            if p == 0 {
                f32::INFINITY
            } else {
                0.5 * (p + 1) as f32 + k as f32
            }
        })
        .collect()
}

fn frame_time_us(k: u32) -> u64 {
    BASE_US + 16_666 * u64::from(k)
}

// ---------------------------------------------------------------------------------------------
// Peer state

#[derive(Default)]
struct Outcome {
    failures: Vec<String>,
    peer_game: Option<String>,
    protocol: Option<u16>,
    session: Option<u32>,
    spawns: Vec<u32>,
    despawns: Vec<u32>,
    cameras: u32,
    poses: u32,
    collision_triangles: usize,
    frames_ok: u32,
    frames_bad: u32,
    focus: Vec<String>,
    lost_reason: Option<String>,
    /// The link's counters when the rounds are over (before the despawns and the bye): a datagram of the
    /// guest that is still in flight when the session ends is legitimately "foreign" to a host that is no
    /// longer in a session.
    stats: Option<Stats>,
}

impl Outcome {
    fn fail(&mut self, what: impl Into<String>) {
        let what = what.into();
        eprintln!("FAIL: {what}");
        self.failures.push(what);
    }
}

struct Peer {
    link: Link,
    started: Instant,
    r: Outcome,
    cameras: VecDeque<(Header, Camera)>,
    poses: VecDeque<Vec<Pose>>,
    frames: VecDeque<FrameReady>,
    collision: Option<Collision>,
    channel: Option<FrameChannel>,
    lost: bool,
    connected: bool,
    spawn_seen: bool,
    despawn_seen: bool,
    focus_grant: bool,
    focus_request: bool,
    focus_back: bool,
}

impl Peer {
    fn new(link: Link) -> Self {
        Self {
            link,
            started: Instant::now(),
            r: Outcome::default(),
            cameras: VecDeque::new(),
            poses: VecDeque::new(),
            frames: VecDeque::new(),
            collision: None,
            channel: None,
            lost: false,
            connected: false,
            spawn_seen: false,
            despawn_seen: false,
            focus_grant: false,
            focus_request: false,
            focus_back: false,
        }
    }

    fn pump(&mut self) {
        for e in self.link.poll() {
            match e {
                Event::Connected(p) => {
                    self.connected = true;
                    self.r.peer_game = Some(p.game);
                    self.r.protocol = Some(p.protocol);
                    self.r.session = Some(p.session);
                }
                Event::Lost { reason } => {
                    self.lost = true;
                    self.r.lost_reason = Some(reason);
                }
                Event::HandshakeFailed { reason } => {
                    self.r.fail(format!("handshake failed: {reason}"))
                }
                Event::Camera { header, camera } => self.cameras.push_back((header, camera)),
                Event::Poses { poses, .. } => self.poses.push_back(poses),
                Event::Collision(c) => self.collision = Some(c),
                Event::Frame { frame, .. } => self.frames.push_back(frame),
                Event::Spawn(s) => {
                    self.r.spawns.push(s.entity);
                    self.spawn_seen = true;
                }
                Event::Despawn { entity } => {
                    self.r.despawns.push(entity);
                    self.despawn_seen = true;
                }
                Event::Focus { focus, requested } => {
                    let kind = if requested { "request" } else { "set" };
                    self.r.focus.push(format!("{}:{kind}", focus.as_str()));
                    match (self.link.role(), focus, requested) {
                        (Role::Host, Role::Host, true) => self.focus_request = true,
                        (Role::Guest, Role::Guest, false) => self.focus_grant = true,
                        (Role::Guest, Role::Host, false) => self.focus_back = true,
                        _ => {}
                    }
                }
                Event::FrameChannel(c) => self.channel = Some(c),
            }
        }
    }

    /// Polls until `done` holds; false after the step limit, the total limit, or a lost session.
    fn wait(&mut self, what: &str, done: impl Fn(&Self) -> bool) -> bool {
        let end = Instant::now() + STEP;
        loop {
            self.pump();
            if done(self) {
                return true;
            }
            if self.lost {
                self.r
                    .fail(format!("session lost while waiting for {what}"));
                return false;
            }
            if Instant::now() > end || self.started.elapsed() > TOTAL {
                self.r.fail(format!("timed out waiting for {what}"));
                return false;
            }
            sleep(Duration::from_millis(2));
        }
    }

    fn for_ms(&mut self, ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            self.pump();
            sleep(Duration::from_millis(2));
        }
    }

    fn connect(&mut self, peer_prefix: &str) -> bool {
        // Waiting for the other process to start counts against the total limit only.
        let end = self.started + TOTAL;
        while !self.connected {
            self.pump();
            if Instant::now() > end {
                self.r.fail("no peer connected");
                return false;
            }
            sleep(Duration::from_millis(2));
        }
        if self.r.protocol != Some(1) {
            self.r
                .fail(format!("protocol {:?}, expected 1", self.r.protocol));
        }
        match &self.r.peer_game {
            Some(g) if g.starts_with(peer_prefix) => {}
            other => self.r.fail(format!(
                "peer game {other:?} does not start with {peer_prefix:?}"
            )),
        }
        true
    }

    fn check_pose(&mut self, entity: u32, k: u32) {
        let Some(p) = self.poses.pop_front() else {
            return;
        };
        if p != vec![pose(entity, k)] {
            self.r
                .fail(format!("pose {k} of entity {entity}: got {p:?}"));
        } else {
            self.r.poses += 1;
        }
    }
}

fn spawn_of(entity: u32) -> Spawn {
    if entity == HOST_ENTITY {
        Spawn {
            entity,
            provider: "War Thunder".into(),
            asset: "/interop/tank".into(),
            name: Some("Host Tank".into()),
        }
    } else {
        Spawn {
            entity,
            provider: "Warframe".into(),
            asset: "/Lotus/Characters/Excalibur".into(),
            name: Some("Excalibur".into()),
        }
    }
}

fn run_host(port: u16, label: &str) -> Peer {
    let cfg = LinkConfig {
        control_port: port,
        heartbeat: Duration::from_millis(100),
        timeout: Duration::from_millis(3000),
        ..LinkConfig::new(format!("interop host ({label})"))
    };
    let mut p = Peer::new(Link::host(cfg).expect("listen"));
    if !p.connect("interop guest (") {
        return p;
    }
    p.link.spawn(&spawn_of(HOST_ENTITY)).expect("spawn");
    p.link.set_focus(Role::Guest).expect("focus");
    if !p.wait("the guest's spawn, pixel file and focus request", |p| {
        p.spawn_seen && p.channel.is_some() && p.focus_request
    }) {
        return p;
    }
    p.link.set_focus(Role::Host).expect("focus");
    let channel = p.channel.clone().expect("channel");
    if (channel.width, channel.height, channel.slots) != (FRAME_W, FRAME_H, SLOTS) {
        p.r.fail(format!("frame_channel {channel:?}"));
        return p;
    }
    let reader = match FrameReader::open(Path::new(&channel.path)) {
        Ok(r) => r,
        Err(e) => {
            p.r.fail(format!("cannot open the pixel file: {e}"));
            return p;
        }
    };
    if !reader.has_depth() {
        p.r.fail("the pixel file has no depth plane");
    }
    for k in 1..=ROUNDS {
        let seq = p.link.send_camera(&camera(k)).expect("camera");
        p.link.send_poses(&[pose(HOST_ENTITY, k)]).expect("pose");
        if k == 1 {
            p.link
                .send_collision([1.0, 2.0, 3.0], 50.0, &triangles())
                .expect("collision");
        }
        if !p.wait(&format!("pose and frame {k}"), |p| {
            !p.poses.is_empty() && !p.frames.is_empty()
        }) {
            return p;
        }
        p.check_pose(GUEST_ENTITY, k);
        let ready = p.frames.pop_front().expect("frame");
        let (mut buf, mut dep) = (Vec::new(), Vec::new());
        match reader.read_depth(&ready, &mut buf, &mut dep) {
            Err(miss) => {
                p.r.frames_bad += 1;
                p.r.fail(format!("frame {k} ({ready:?}) not readable: {miss:?}"));
            }
            Ok(d) => {
                let ok = ready.frame == u64::from(k)
                    && d.frame == u64::from(k)
                    && d.camera_seq == seq
                    && (d.width, d.height) == (FRAME_W, FRAME_H)
                    && d.time_us == frame_time_us(k)
                    && buf == rgba(k)
                    && dep.len() == depth(k).len()
                    && dep
                        .iter()
                        .zip(depth(k))
                        .all(|(a, b)| a.to_bits() == b.to_bits());
                if ok {
                    p.r.frames_ok += 1;
                } else {
                    p.r.frames_bad += 1;
                    p.r.fail(format!(
                        "frame {k}: announced {ready:?}, header {d:?} (camera seq sent {seq}), colour ok {}, depth ok {}",
                        buf == rgba(k),
                        dep == depth(k)
                    ));
                }
            }
        }
    }
    p.r.stats = Some(p.link.stats());
    p.link.despawn(HOST_ENTITY).expect("despawn");
    if !p.wait("the guest's despawn", |p| p.despawn_seen) {
        return p;
    }
    p.wait("the guest's bye", |p| p.lost);
    p
}

fn run_guest(port: u16, label: &str) -> Peer {
    let cfg = LinkConfig {
        control_port: port,
        heartbeat: Duration::from_millis(100),
        timeout: Duration::from_millis(3000),
        reconnect: Duration::from_millis(100),
        ..LinkConfig::new(format!("interop guest ({label})"))
    };
    let mut p = Peer::new(Link::guest(cfg).expect("guest"));
    if !p.connect("interop host (") {
        return p;
    }
    let opts = FrameFileOptions::new(FRAME_W, FRAME_H, SLOTS).with_depth();
    let mut writer = match FrameWriter::create_temp_with(&format!("interop-{label}"), &opts) {
        Ok(w) => w,
        Err(e) => {
            p.r.fail(format!("cannot create the pixel file: {e}"));
            return p;
        }
    };
    p.link.spawn(&spawn_of(GUEST_ENTITY)).expect("spawn");
    p.link.announce_frames(writer.channel()).expect("announce");
    if !p.wait("the host's spawn and focus", |p| {
        p.spawn_seen && p.focus_grant
    }) {
        return p;
    }
    p.link.set_focus(Role::Host).expect("request focus");
    if !p.wait("the focus coming back", |p| p.focus_back) {
        return p;
    }
    for k in 1..=ROUNDS {
        if !p.wait(&format!("camera {k}"), |p| !p.cameras.is_empty()) {
            return p;
        }
        let (header, cam) = p.cameras.pop_front().expect("camera");
        if cam != camera(k) {
            p.r.fail(format!("camera {k}: got {cam:?}"));
        } else {
            p.r.cameras += 1;
        }
        p.link.send_poses(&[pose(GUEST_ENTITY, k)]).expect("pose");
        let ready = writer
            .write_depth(
                &rgba(k),
                Some(&depth(k)),
                FRAME_W,
                FRAME_H,
                header.seq,
                frame_time_us(k),
            )
            .expect("write");
        p.link.send_frame(&ready).expect("frame");
        if !p.wait(&format!("the host's pose {k}"), |p| !p.poses.is_empty()) {
            return p;
        }
        p.check_pose(HOST_ENTITY, k);
        if k == 1 {
            if !p.wait("the collision snapshot", |p| p.collision.is_some()) {
                return p;
            }
            let c = p.collision.take().expect("collision");
            p.r.collision_triangles = c.triangles.len();
            if c.center != [1.0, 2.0, 3.0] || c.radius != 50.0 || c.triangles != triangles() {
                p.r.fail("the collision snapshot differs from the script");
            }
        }
    }
    if !p.wait("the host's despawn", |p| p.despawn_seen) {
        return p;
    }
    p.r.stats = Some(p.link.stats());
    p.link.despawn(GUEST_ENTITY).expect("despawn");
    p.link.disconnect("done");
    p.for_ms(200);
    if p.lost {
        p.r.fail("the guest got a Lost event for its own disconnect");
    }
    p
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let role = args.first().map(String::as_str);
    let (Some(port), Some(label)) = (
        value("--port").and_then(|p| p.parse::<u16>().ok()),
        value("--label"),
    ) else {
        eprintln!("usage: interop_peer host|guest --port PORT --label LABEL");
        exit(2);
    };
    let mut p = match role {
        Some("host") => run_host(port, &label),
        Some("guest") => run_guest(port, &label),
        _ => {
            eprintln!("usage: interop_peer host|guest --port PORT --label LABEL");
            exit(2);
        }
    };
    p.pump();
    if role == Some("host") {
        if p.r.frames_ok != ROUNDS {
            p.r.fail(format!("frames verified: {} of {ROUNDS}", p.r.frames_ok));
        }
        match &p.r.lost_reason {
            Some(r) if r.contains("bye") => {}
            other => {
                p.r.fail(format!("lost reason {other:?} does not mention bye"))
            }
        }
    } else if p.r.cameras != ROUNDS {
        p.r.fail(format!("cameras verified: {} of {ROUNDS}", p.r.cameras));
    }
    let s = p.r.stats.unwrap_or_else(|| p.link.stats());
    let result = json!({
        "role": role, "label": label, "ok": p.r.failures.is_empty(), "failures": p.r.failures,
        "peer_game": p.r.peer_game, "protocol": p.r.protocol, "session": p.r.session,
        "spawns": p.r.spawns, "despawns": p.r.despawns, "cameras": p.r.cameras, "poses": p.r.poses,
        "collision_triangles": p.r.collision_triangles, "frames_ok": p.r.frames_ok, "frames_bad": p.r.frames_bad,
        "focus": p.r.focus, "lost_reason": p.r.lost_reason,
        "stats": {"malformed": s.malformed, "foreign": s.foreign, "stale": s.stale,
                  "unknown_entity": s.unknown_entity, "spawn_ignored": s.spawn_ignored},
    });
    let ok = p.r.failures.is_empty();
    drop(p);
    println!("RBL-INTEROP-RESULT {result}");
    exit(if ok { 0 } else { 1 });
}
