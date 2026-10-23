//! Regression tests for the code-review findings on the passthrough link (contract v1, revision
//! 4). Each test names the finding it covers and fails on the code that review was written against.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use rewrite_bridge::link::*;

fn cfg(game: &str, control_port: u16) -> LinkConfig {
    LinkConfig {
        control_port,
        heartbeat: Duration::from_millis(50),
        timeout: Duration::from_millis(3000),
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
        let host = Link::host(cfg("H", 0)).unwrap();
        let port = host.control_addr().unwrap().port();
        let guest = Link::guest(cfg("G", port)).unwrap();
        let mut p = Self {
            host,
            guest,
            he: vec![],
            ge: vec![],
        };
        assert!(p.until(3000, |p| p.host.is_connected() && p.guest.is_connected()));
        p
    }

    fn pump(&mut self) {
        self.he.extend(self.host.poll());
        self.ge.extend(self.guest.poll());
    }

    /// Polls both sides until `f` holds, for at most `ms` milliseconds.
    fn until(&mut self, ms: u64, f: impl Fn(&Self) -> bool) -> bool {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            self.pump();
            if f(self) {
                return true;
            }
            sleep(Duration::from_millis(2));
        }
        false
    }

    fn for_ms(&mut self, ms: u64) {
        self.until(ms, |_| false);
    }
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("review-{}-{name}", std::process::id()))
}

fn frames_in(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, Event::Frame { .. }))
        .count()
}

fn spawn(entity: u32) -> Spawn {
    Spawn {
        entity,
        provider: "P".into(),
        asset: "a".into(),
        name: None,
    }
}

fn poses_for(events: &[Event], entity: u32) -> usize {
    events
        .iter()
        .map(|e| match e {
            Event::Poses { poses, .. } => poses.iter().filter(|p| p.entity == entity).count(),
            _ => 0,
        })
        .sum()
}

// ----- M1: frame numbering across pixel files

/// A second pixel file (a resize) restarts frame numbers at 1; the host's staleness check used to
/// drop every frame until the numbers passed the old file's.
#[test]
fn m1_frames_of_a_new_pixel_file_are_not_stale() {
    let mut p = Pair::new();
    let mut w1 = FrameWriter::create_temp("m1a", 16, 8, 3).unwrap();
    p.guest.announce_frames(w1.channel()).unwrap();
    let rgba = vec![255u8; 16 * 8 * 4];
    for _ in 0..10 {
        let r = w1.write(&rgba, 16, 8, 0, now_us()).unwrap();
        p.guest.send_frame(&r).unwrap();
        p.for_ms(5);
    }
    assert!(
        p.until(1000, |p| frames_in(&p.he) == 10),
        "10 frames of file 1"
    );
    p.he.clear();

    let mut w2 = FrameWriter::create_temp("m1b", 32, 16, 3).unwrap();
    p.guest.announce_frames(w2.channel()).unwrap();
    p.for_ms(30); // the host reads the new announcement
    let rgba2 = vec![255u8; 32 * 16 * 4];
    for _ in 0..5 {
        let r = w2.write(&rgba2, 32, 16, 0, now_us()).unwrap();
        p.guest.send_frame(&r).unwrap();
        p.for_ms(20);
    }
    assert_eq!(
        frames_in(&p.he),
        5,
        "frames 1..5 of the new channel; stale counter {}",
        p.host.stats().stale
    );
}

// ----- M2: collision bursts

fn big_snapshot(chunks: usize) -> Vec<Triangle> {
    (0..chunks * MAX_TRIANGLES)
        .map(|i| {
            let x = i as f32;
            [[x, 0.0, 0.0], [x + 1.0, 0.0, 0.0], [x, 0.0, -1.0]]
        })
        .collect()
}

/// A 400-chunk snapshot (3.2 MB) sent in one call arrived whole only when the receiver's buffer
/// happened to hold it all.
#[test]
fn m2_a_400_chunk_collision_snapshot_completes() {
    let mut p = Pair::new();
    let tris = big_snapshot(400);
    let q = p.host.send_collision([0.0; 3], 16.0, &tris).unwrap();
    let done = p.until(15_000, |p| {
        p.ge.iter().any(|e| matches!(e, Event::Collision(_)))
    });
    assert!(
        done,
        "snapshot never completed; guest stats {:?}",
        p.guest.stats()
    );
    let Some(Event::Collision(c)) = p.ge.iter().find(|e| matches!(e, Event::Collision(_))) else {
        unreachable!()
    };
    assert_eq!((c.query, c.triangles.len()), (q, tris.len()));
    assert_eq!(c.triangles.first(), tris.first());
    assert_eq!(c.triangles.last(), tris.last());
}

/// `send_collision` queues; a poll sends at most 32 collision datagrams.
#[test]
fn m2_collision_chunks_are_paced_per_poll() {
    let mut p = Pair::new();
    p.for_ms(100);
    let tris = big_snapshot(400);
    let before = p.host.stats().datagrams_sent;
    p.host.send_collision([0.0; 3], 16.0, &tris).unwrap();
    p.he.extend(p.host.poll());
    let sent = p.host.stats().datagrams_sent - before;
    // 32 chunks, and possibly a heartbeat that fell due in the same poll.
    assert!(
        (1..=33).contains(&sent),
        "one send_collision and one poll put {sent} datagrams on the wire"
    );
}

/// A newer query replaces the chunks of an older one that are still queued.
#[test]
fn m2_a_newer_query_replaces_queued_chunks() {
    let mut p = Pair::new();
    p.for_ms(100);
    let before = p.host.stats().datagrams_sent;
    p.host
        .send_collision([0.0; 3], 16.0, &big_snapshot(400))
        .unwrap();
    let small = big_snapshot(2);
    let q2 = p.host.send_collision([1.0; 3], 8.0, &small).unwrap();
    assert!(p.until(5000, |p| {
        p.ge.iter()
            .any(|e| matches!(e, Event::Collision(c) if c.query == q2))
    }));
    p.for_ms(100);
    let sent = p.host.stats().datagrams_sent - before;
    assert!(
        sent < 60,
        "the 400 chunks of the replaced query were sent anyway: {sent} datagrams"
    );
    let queries: Vec<u32> =
        p.ge.iter()
            .filter_map(|e| match e {
                Event::Collision(c) => Some(c.query),
                _ => None,
            })
            .collect();
    assert_eq!(queries, [q2]);
}

// ----- M3 / M4 / M5 / M7: opening, reading and creating pixel files

/// A FIFO named by the peer blocked `open` for ever.
#[cfg(unix)]
#[test]
fn m3_a_fifo_does_not_hang_open() {
    let p = tmp("fifo");
    let _ = std::fs::remove_file(&p);
    assert!(std::process::Command::new("mkfifo")
        .arg(&p)
        .status()
        .unwrap()
        .success());
    let (tx, rx) = std::sync::mpsc::channel();
    let p2 = p.clone();
    std::thread::spawn(move || {
        let r = FrameReader::open(&p2);
        let _ = tx.send(r.is_ok());
    });
    let got = rx.recv_timeout(Duration::from_secs(2));
    if got.is_err() {
        // Unblock a thread that is still stuck in open(), so the test process can exit. Opening a
        // FIFO for reading and writing never waits for the other end.
        let _ = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&p)
            .map(|mut f| f.write_all(b"x"));
    }
    let _ = std::fs::remove_file(&p);
    assert_eq!(
        got,
        Ok(false),
        "open() on a FIFO must return an error at once"
    );
}

/// A valid pixel file, as bytes, from a writer (which deletes its own file when dropped).
fn valid_file(w: u32, h: u32, slots: u32) -> Vec<u8> {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = tmp(&format!("src-{w}x{h}-{n}.frames"));
    let wr = FrameWriter::create(&path, w, h, slots).unwrap();
    let bytes = std::fs::read(wr.path()).unwrap();
    drop(wr);
    bytes
}

fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

// The relative spelling is built from `..` components, which needs a root without a drive prefix.
#[cfg(unix)]
#[test]
fn m3_a_path_that_is_not_absolute_is_refused() {
    let abs = tmp("rel.frames");
    std::fs::write(&abs, valid_file(4, 4, 2)).unwrap();
    // The same file, spelt relative to the working directory.
    let cwd = std::env::current_dir().unwrap();
    let mut rel = PathBuf::new();
    for _ in cwd.components().skip(1) {
        rel.push("..");
    }
    for c in abs.components().skip(1) {
        rel.push(c);
    }
    assert!(rel.is_relative());
    assert!(
        std::fs::metadata(&rel).is_ok(),
        "the relative spelling reaches the file"
    );
    assert!(FrameReader::open(&abs).is_ok());
    let err = FrameReader::open(&rel).err();
    let _ = std::fs::remove_file(&abs);
    assert!(err.is_some(), "a relative path must be refused");
}

#[cfg(unix)]
#[test]
fn m3_a_symlink_or_a_directory_is_not_a_regular_file() {
    let real = tmp("real.frames");
    std::fs::write(&real, valid_file(4, 4, 2)).unwrap();
    let link = tmp("link.frames");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let via_link = FrameReader::open(&link).err();
    let dir = FrameReader::open(&std::env::temp_dir()).err();
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_file(&real);
    assert!(via_link.is_some(), "a symlink is refused");
    assert!(dir.is_some(), "a directory is refused");
}

/// Readers refuse a width or height above 16384, whatever the file's size says.
#[test]
fn m4_oversized_dimensions_are_refused() {
    let path = tmp("dims.frames");
    for (w, h, ok) in [
        (16384u32, 1u32, true),
        (1, 16384, true),
        (16385, 1, false),
        (1, 16385, false),
        (65536, 1, false),
        (1, 65536, false),
    ] {
        let mut b = valid_file(1, 1, 1);
        let stride = w * 4;
        let slot = (64 + u64::from(stride) * u64::from(h)).div_ceil(64) * 64;
        put_u32(&mut b, 16, w);
        put_u32(&mut b, 20, h);
        put_u32(&mut b, 24, stride);
        b[32..40].copy_from_slice(&slot.to_le_bytes());
        b.resize(64 + slot as usize, 0);
        std::fs::write(&path, &b).unwrap();
        let r = FrameReader::open(&path);
        assert_eq!(r.is_ok(), ok, "{w}x{h}: {:?}", r.err());
    }
    let _ = std::fs::remove_file(&path);
}

/// The contract says `stride = width * 4`; the Rust reader used to accept anything larger.
#[test]
fn stride_must_equal_width_times_four() {
    let path = tmp("stride.frames");
    let mut b = valid_file(8, 4, 2);
    std::fs::write(&path, &b).unwrap();
    assert!(FrameReader::open(&path).is_ok());
    put_u32(&mut b, 24, 8 * 4 + 4);
    // Keep slot_size big enough for the wider rows, so only the stride rule can refuse it.
    let slot = (64 + (8u64 * 4 + 4) * 4).div_ceil(64) * 64 + 64;
    b[32..40].copy_from_slice(&slot.to_le_bytes());
    b.resize(64 + 2 * slot as usize, 0);
    std::fs::write(&path, &b).unwrap();
    let r = FrameReader::open(&path);
    let _ = std::fs::remove_file(&path);
    assert!(r.is_err(), "stride != width * 4 is malformed");
}

/// A sender never reuses a path: creating over an existing file fails and leaves it alone.
#[test]
fn m7_a_writer_never_reuses_a_path() {
    let path = tmp("exists.frames");
    std::fs::write(&path, b"somebody else's file").unwrap();
    let r = FrameWriter::create(&path, 4, 4, 2);
    let kept = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let err = r.err().expect("create over an existing file must fail");
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(kept, b"somebody else's file");
}

/// The peer truncating its file under a mapped reader killed the process with SIGBUS. The test runs
/// the dangerous part in a child process and wants it to survive and report the frame as gone.
#[cfg(unix)]
#[test]
fn m5_a_truncated_pixel_file_is_gone_not_a_crash() {
    if let Ok(file) = std::env::var("REVIEW_FILE") {
        let p = PathBuf::from(file);
        let mut w = FrameWriter::create(&p, 64, 64, 2).unwrap();
        let rgba = vec![7u8; 64 * 64 * 4];
        let ready = w.write(&rgba, 64, 64, 0, 1).unwrap();
        let r = FrameReader::open(&p).unwrap();
        // The peer (here: this process, through another handle) truncates the file after the map.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_len(64)
            .unwrap();
        let mut out = Vec::new();
        println!(
            "CHILD-RESULT {:?}",
            r.read(&ready, &mut out).map(|d| d.frame)
        );
        std::mem::forget(w); // dropping would only delete the file
        let _ = std::fs::remove_file(&p);
        return;
    }
    let p = tmp("trunc.frames");
    let _ = std::fs::remove_file(&p);
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "m5_a_truncated_pixel_file_is_gone_not_a_crash",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("REVIEW_FILE", &p)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&p);
    let stdout = String::from_utf8_lossy(&out.stdout);
    {
        use std::os::unix::process::ExitStatusExt;
        assert!(
            out.status.success(),
            "the child died (signal {:?}): {stdout}",
            out.status.signal()
        );
    }
    assert!(
        stdout.contains("CHILD-RESULT Err("),
        "expected an error result, got: {stdout}"
    );
}

// ----- M6: entity tables

/// Poses for ids the peer never spawned are ignored; at most 1024 live spawned entities per peer.
#[test]
fn m6_only_spawned_entities_have_pose_state() {
    let mut p = Pair::new();
    p.guest.spawn(&spawn(5)).unwrap();
    assert!(p.until(1000, |p| p.he.iter().any(|e| matches!(e, Event::Spawn(_)))));
    let t = now_us();
    p.guest
        .send_poses(&[Pose::new(5, t), Pose::new(6, t), Pose::new(7, t)])
        .unwrap();
    p.for_ms(100);
    assert_eq!(poses_for(&p.he, 5), 1, "the spawned entity");
    assert_eq!(
        poses_for(&p.he, 6) + poses_for(&p.he, 7),
        0,
        "never spawned"
    );
    assert_eq!(p.host.stats().unknown_entity, 2);

    // After a despawn the id is unknown again.
    p.guest.despawn(5).unwrap();
    p.for_ms(50);
    p.guest.send_poses(&[Pose::new(5, t + 10)]).unwrap();
    p.for_ms(100);
    assert_eq!(poses_for(&p.he, 5), 1, "no pose state after despawn");
    assert_eq!(p.host.stats().unknown_entity, 3);
}

#[test]
fn m6_at_most_1024_live_spawned_entities_per_peer() {
    let mut p = Pair::new();
    for e in 0..1030 {
        p.guest.spawn(&spawn(e)).unwrap();
        if e % 100 == 99 {
            p.for_ms(5);
        }
    }
    p.for_ms(200);
    let spawns = p.he.iter().filter(|e| matches!(e, Event::Spawn(_))).count();
    assert_eq!(spawns, 1024, "the spawns beyond the table are ignored");
    assert_eq!(p.host.stats().spawn_ignored, 6);
    let t = now_us();
    p.guest
        .send_poses(&[Pose::new(0, t), Pose::new(1023, t), Pose::new(1024, t)])
        .unwrap();
    p.for_ms(100);
    assert_eq!(poses_for(&p.he, 0), 1);
    assert_eq!(poses_for(&p.he, 1023), 1);
    assert_eq!(poses_for(&p.he, 1024), 0, "its spawn was ignored");
    assert_eq!(p.host.stats().unknown_entity, 1);

    // A despawn frees a place; re-spawning a live id is a replacement, not a new entity.
    p.guest.despawn(0).unwrap();
    p.guest.spawn(&spawn(1)).unwrap();
    p.guest.spawn(&spawn(2000)).unwrap();
    p.for_ms(100);
    p.guest
        .send_poses(&[Pose::new(2000, t + 5), Pose::new(1, t + 5)])
        .unwrap();
    p.for_ms(100);
    assert_eq!(poses_for(&p.he, 2000), 1, "a place was free");
    assert_eq!(poses_for(&p.he, 1), 1);
    let respawned =
        p.he.iter()
            .filter(|e| matches!(e, Event::Spawn(s) if s.entity == 1))
            .count();
    assert_eq!(
        respawned, 2,
        "a spawn of a live id replaces it, even in a full table"
    );
}

// ----- L9: heartbeat_ms

/// A raw guest says hello with the given heartbeat; returns what the host does with it.
fn hello_with_heartbeat(ms: u64) -> Vec<Event> {
    let mut host = Link::host(cfg("H", 0)).unwrap();
    let port = host.control_addr().unwrap().port();
    let mut tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    writeln!(
        tcp,
        r#"{{"type":"hello","role":"guest","game":"raw","protocol":1,"min_protocol":1,"udp_port":9,"heartbeat_ms":{ms},"timeout_ms":2000}}"#
    )
    .unwrap();
    let mut events = vec![];
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end && events.is_empty() {
        events.extend(host.poll());
        sleep(Duration::from_millis(2));
    }
    let mut r = BufReader::new(tcp.try_clone().unwrap());
    tcp.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let mut line = String::new();
    for _ in 0..50 {
        host.poll();
        sleep(Duration::from_millis(2));
    }
    r.read_line(&mut line).unwrap(); // the host's hello, or its bye
    if events
        .iter()
        .any(|e| matches!(e, Event::HandshakeFailed { .. }))
    {
        let mut bye = String::new();
        r.read_line(&mut bye).unwrap();
        assert!(
            bye.contains("heartbeat_ms") || line.contains("heartbeat_ms"),
            "the bye says why: {line:?} {bye:?}"
        );
    }
    events
}

#[test]
fn l9_heartbeat_ms_must_be_10_to_10000() {
    for ms in [0u64, 1, 9, 10_001, 3_600_000, u64::from(u32::MAX)] {
        let ev = hello_with_heartbeat(ms);
        assert!(
            matches!(&ev[..], [Event::HandshakeFailed { reason }] if reason.contains("heartbeat_ms")),
            "heartbeat_ms {ms}: {ev:?}"
        );
    }
    for ms in [10u64, 250, 10_000] {
        let ev = hello_with_heartbeat(ms);
        assert!(
            matches!(&ev[..], [Event::Connected(_)]),
            "heartbeat_ms {ms}: {ev:?}"
        );
    }
}

// ----- L7: the JSON dialect of the control channel

/// Documents and whether the Rust side (serde_json) accepts them. link-java's `Json` reader must
/// give the same answers: `LinkTests` carries this very table. Control lines are parsed by two
/// implementations, so a line one accepts and the other rejects is a way to desynchronise them.
pub const JSON_CORPUS: &[(&str, bool)] = &[
    // numbers
    ("0", true),
    ("-0", true),
    ("-1", true),
    ("1.5", true),
    ("1e5", true),
    ("1E+5", true),
    ("1e-5", true),
    ("0e0", true),
    ("-0.0", true),
    ("1e-999", true),
    ("123456789012345678901234567890", true),
    ("01", false),
    ("-01", false),
    ("00", false),
    ("1.", false),
    ("1.e5", false),
    ("-.5", false),
    (".5", false),
    ("1e", false),
    ("1.0e+", false),
    ("+1", false),
    ("-", false),
    ("- 1", false),
    ("0x10", false),
    ("1e999", false),
    ("Infinity", false),
    ("NaN", false),
    // string escapes
    (r#""A""#, true),
    (r#""é""#, true),
    (r#""\u0000""#, true),
    (r#""\/""#, true),
    (r#""𐀀""#, true),
    (r#""😀""#, true),
    (r#""𝄞""#, true),
    (r#""\u004""#, false),
    (r#""\u00zz""#, false),
    (r#""\u+041""#, false),
    (r#""\u-041""#, false),
    (r#""\u D800""#, false),
    (r#""\ud800""#, false),
    (r#""\udc00""#, false),
    (r#""\ud800x""#, false),
    (r#""\ud800A""#, false),
    (r#""\ud800\ud800""#, false),
    (r#""\udc00\ud800""#, false),
    (r#""\x41""#, false),
    (r#""\U0041""#, false),
    ("\"a\nb\"", false),
    // structure
    ("true", true),
    ("[true,false,null]", true),
    ("\t1\n", true),
    ("truex", false),
    ("tru", false),
    ("[1,]", false),
    ("[,1]", false),
    (r#"{"a":1,}"#, false),
    (r#"{"a":1 "b":2}"#, false),
    ("{'a':1}", false),
    ("\u{feff}1", false),
];

/// How deep arrays and objects may nest: 127 levels, the 128th is an error.
const JSON_MAX_NESTING: usize = 127;

#[test]
fn l7_json_corpus_matches_serde_json() {
    for &(doc, ok) in JSON_CORPUS {
        let got = serde_json::from_str::<serde_json::Value>(doc).is_ok();
        assert_eq!(got, ok, "{doc:?}");
    }
    for n in [1, JSON_MAX_NESTING, JSON_MAX_NESTING + 1, 500] {
        let arrays = format!("{}{}", "[".repeat(n), "]".repeat(n));
        let objects = format!("{}1{}", "{\"a\":".repeat(n), "}".repeat(n));
        for doc in [arrays, objects] {
            let got = serde_json::from_str::<serde_json::Value>(&doc).is_ok();
            assert_eq!(got, n <= JSON_MAX_NESTING, "nesting {n}");
        }
    }
}
