//! A Warframe rewrite's ids on the wire, against the real `fake_host` and `fake_guest` programs.
//!
//! The ids are in the format `wf-unpack`'s provider lists: a model path, a model path with `?animations=`
//! and a clip filter, a `.level` path with and without `?collision`. The tests need no game files: the link
//! carries an id as a string, and the fake peers only log and count it. What the ids *mean* (that
//! `Warframe::load` finds each one) is wf-unpack's to prove; the ones below were each checked against an
//! install with `wf-unpack resolve` (see docs/INTEGRATING.md, "Warframe").
//!
//! One side of each test is the example program, run as a child process, and the other is a `Link` in
//! this process, so what is asserted is what the contract says a receiver does:
//! - a `spawn` is kept as it came, whatever the asset string is (section `spawn`);
//! - a `Pose` for an entity that was never spawned, or was despawned, is ignored and counted in
//!   `Stats::unknown_entity`, and does not change the state of the entities that are live.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use rewrite_bridge::link::*;

/// A skinned character, listed by the provider as a `Character`.
const CHARACTER: &str = "/Lotus/Characters/Tenno/Excalibur/Excalibur_skel.fbx";
/// The same model with the clips of one stance: the animated id.
const ANIMATED: &str =
    "/Lotus/Characters/Tenno/Excalibur/Excalibur_skel.fbx?animations=/Lotus/Animations/Tenno/Movement/MeleeWeapon/Sword/";
/// A weapon, listed as a `Weapon`.
const WEAPON: &str = "/Lotus/Weapons/Tenno/Melee/Swords/PrimeGalatine/PrimeGalatine.fbx";
/// A level, and its collision as a scene of its own.
const LEVEL: &str = "/Lotus/Levels/ClanDojo/ClanObstacleCourseA.level";
const LEVEL_COLLISION: &str = "/Lotus/Levels/ClanDojo/ClanObstacleCourseA.level?collision";
/// An entity name of the game's data, which is not an asset id (wf-unpack's `load` refuses it).
const UNIQUE_NAME: &str = "/Lotus/Powersuits/Excalibur/Excalibur";

/// A child process that is killed when the test ends, however it ends.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The path of an example program of this package, built first: `cargo test` builds the examples, but
/// running one test target alone does not, and a program left over from an older build would be tested
/// instead. Building an up-to-date example takes a moment.
fn example(name: &str) -> PathBuf {
    let mut dir = std::env::current_exe().unwrap();
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut build = Command::new(cargo);
    build.args(["build", "--features", "link", "--example", name]);
    if dir.ends_with("release") {
        build.arg("--release");
    }
    let status = build
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .unwrap();
    assert!(status.success(), "could not build the {name} example");
    let exe = dir
        .join("examples")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(exe.exists(), "{} is not there", exe.display());
    exe
}

/// Starts an example with its output read line by line on a channel.
fn start(name: &str, args: &[&str]) -> (KillOnDrop, Receiver<String>) {
    let mut child = Command::new(example(name))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (KillOnDrop(child), rx)
}

/// Reads lines until one satisfies `want`, for at most `secs` seconds; returns it, with everything read.
fn wait_for(
    rx: &Receiver<String>,
    log: &mut Vec<String>,
    secs: u64,
    want: impl Fn(&str) -> bool,
) -> String {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => {
                log.push(line.clone());
                if want(&line) {
                    return line;
                }
            }
            Err(_) => panic!("timed out; the program printed:\n{}", log.join("\n")),
        }
    }
}

fn cfg(game: &str, port: u16) -> LinkConfig {
    LinkConfig {
        control_port: port,
        heartbeat: Duration::from_millis(50),
        timeout: Duration::from_millis(1000),
        reconnect: Duration::from_millis(50),
        ..LinkConfig::new(game)
    }
}

fn pose(entity: u32, time_us: u64, x: f32) -> Pose {
    Pose {
        translation: [x, 0.0, 0.0],
        ..Pose::new(entity, time_us)
    }
}

fn spawn(entity: u32, asset: &str, name: &str) -> Spawn {
    Spawn {
        entity,
        provider: "Warframe".into(),
        asset: asset.into(),
        name: Some(name.into()),
    }
}

/// The number after `key` in a `fake_host` report line (`... unknown_entity 2`).
fn count_after(line: &str, key: &str) -> Option<u64> {
    let rest = line.split(key).nth(1)?;
    rest.split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())?
        .parse()
        .ok()
}

/// A guest of this process against the real `fake_host`: spawn, poses, despawn, and a pose for an id the
/// host never heard of.
#[test]
fn fake_host_takes_warframe_ids_and_counts_poses_of_unknown_entities() {
    let (_host, rx) = start("fake_host", &["--port", "0", "--seconds", "20"]);
    let mut log = Vec::new();
    let listening = wait_for(&rx, &mut log, 10, |l| l.contains("listening on"));
    let port: u16 = listening
        .rsplit(':')
        .next()
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    let mut guest = Link::guest(cfg("Warframe rewrite", port)).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while !guest.is_connected() {
        assert!(
            Instant::now() < end,
            "the guest never connected:\n{}",
            log.join("\n")
        );
        guest.poll();
        std::thread::sleep(Duration::from_millis(5));
    }

    // Four entities: the animated character, a weapon, a level's collision, and a uniqueName that is not an
    // asset id. The link does not look at the asset, so all four are accepted as announced
    let announced = [
        (1, ANIMATED, "Excalibur"),
        (2, WEAPON, "Galatine"),
        (3, LEVEL_COLLISION, "Obstacle course collision"),
        (4, UNIQUE_NAME, "a name, not an id"),
    ];
    for (entity, asset, name) in announced {
        guest.spawn(&spawn(entity, asset, name)).unwrap();
    }
    for (entity, asset, _) in announced {
        let want = format!("guest spawned entity {entity} = Warframe asset \"{asset}\"");
        wait_for(&rx, &mut log, 10, |l| l.contains(&want));
    }

    // Poses of live entities, then one datagram with a live entity and two that were never spawned. The
    // unknown ones come last, so if they were applied the host's last pose would be theirs
    let t = now_us();
    guest
        .send_poses(&[
            pose(1, t, 1.0),
            pose(2, t, 2.0),
            pose(3, t, 3.0),
            pose(4, t, 4.0),
        ])
        .unwrap();
    guest
        .send_poses(&[
            pose(1, t + 1000, 7.5),
            pose(99, t + 1000, -42.0),
            pose(5, t + 1000, -43.0),
        ])
        .unwrap();
    let report = wait_for(&rx, &mut log, 10, |l| {
        l.contains("last pose Some([7.5, 0.0, 0.0])") && count_after(l, "unknown_entity") == Some(2)
    });
    assert!(report.contains("malformed 0"), "{report}");

    // Despawn the weapon: its next pose is unknown too, and the live entity's state is not disturbed
    guest.despawn(2).unwrap();
    wait_for(&rx, &mut log, 10, |l| {
        l.contains("guest despawned entity 2")
    });
    guest.send_poses(&[pose(2, t + 2000, -44.0)]).unwrap();
    guest.send_poses(&[pose(1, t + 3000, 8.5)]).unwrap();
    let report = wait_for(&rx, &mut log, 10, |l| {
        l.contains("last pose Some([8.5, 0.0, 0.0])") && count_after(l, "unknown_entity") == Some(3)
    });
    assert!(report.contains("malformed 0"), "{report}");

    // A second spawn of a live entity replaces the first (the contract's rule), even with another id
    guest
        .spawn(&spawn(1, CHARACTER, "Excalibur (no clips)"))
        .unwrap();
    let want = format!("guest spawned entity 1 = Warframe asset \"{CHARACTER}\"");
    wait_for(&rx, &mut log, 10, |l| l.contains(&want));
}

/// The real `fake_guest`, told to announce a Warframe id, against a host of this process: the host sees the
/// spawn as it was written, and the poses of the entity are live (none is counted unknown).
#[test]
fn fake_guest_announces_a_warframe_id() {
    let mut host = Link::host(cfg("Warframe host", 0)).unwrap();
    let port = host.control_addr().unwrap().port();
    let port_arg = port.to_string();
    let (_guest, _rx) = start(
        "fake_guest",
        &[
            "--port",
            &port_arg,
            "--seconds",
            "20",
            "--provider",
            "Warframe",
            "--asset",
            ANIMATED,
            "--name",
            "Excalibur",
        ],
    );
    let camera = Camera::look_at(
        [0.0, 2.0, 8.0],
        [0.0, 0.9, 0.0],
        1.0,
        0.1,
        100.0,
        [320, 240],
    );
    let end = Instant::now() + Duration::from_secs(10);
    let (mut spawned, mut posed) = (None, 0);
    while Instant::now() < end && (spawned.is_none() || posed == 0) {
        for event in host.poll() {
            match event {
                Event::Spawn(s) => spawned = Some(s),
                Event::Poses { poses, .. } => {
                    posed += poses.iter().filter(|p| p.entity == 1).count()
                }
                _ => {}
            }
        }
        if host.is_connected() {
            let _ = host.send_camera(&camera);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        spawned,
        Some(Spawn {
            entity: 1,
            provider: "Warframe".into(),
            asset: ANIMATED.into(),
            name: Some("Excalibur".into()),
        })
    );
    assert!(posed > 0, "the guest sent no pose of its entity");
    assert_eq!(host.stats().unknown_entity, 0);
    assert_eq!(host.stats().malformed, 0);
}

/// The ids of this file have the shapes wf-unpack lists, so a reader copying them copies real forms.
#[test]
fn the_ids_have_the_forms_the_provider_lists() {
    for id in [CHARACTER, WEAPON, LEVEL] {
        assert!(id.starts_with("/Lotus/"), "{id}");
        assert!(id.ends_with(".fbx") || id.ends_with(".level"), "{id}");
        assert!(!id.contains('?'), "{id}");
    }
    assert_eq!(ANIMATED.split_once("?animations=").unwrap().0, CHARACTER);
    assert_eq!(LEVEL_COLLISION.strip_suffix("?collision"), Some(LEVEL));
    // A uniqueName has no extension: it names an entity, not a file
    assert!(!UNIQUE_NAME.ends_with(".fbx") && !UNIQUE_NAME.ends_with(".level"));
}
