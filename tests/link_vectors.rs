//! Runs the shared link test vectors (`tests/vectors/link`, see the README there) against this
//! implementation: control lines, datagrams, pixel files and scripted sessions over real sockets.
//!
//! The vectors are derived from `docs/CONTRACT.md` alone and are the same files `link-java`, `link-c` and
//! `link-cs` run, so a vector this file fails is a place where this implementation and the contract (or
//! another implementation) disagree. Every failing vector is listed in one assertion at the end of its
//! test, with its id, section and basis.
//!
//! `KNOWN_DISAGREEMENTS` names vectors this implementation is known to fail, with the reason. They are
//! expected to fail: the test also fails when one of them passes, so the list cannot go stale.
//!
//! `RBL_VECTORS_DIR` overrides the directory; `RBL_VECTORS_SKIP_LARGE=1` skips the cases that need a sparse
//! file of up to 1 GiB.

use std::collections::HashMap;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use rewrite_bridge::link::*;
use serde_json::Value;

/// Vectors this implementation fails, `(id, why)`. Empty when it passes them all.
const KNOWN_DISAGREEMENTS: &[(&str, &str)] = &[];

// ---------------------------------------------------------------------------------------------
// Loading, reporting

fn vectors_dir() -> PathBuf {
    match std::env::var_os("RBL_VECTORS_DIR") {
        Some(d) => PathBuf::from(d),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors/link"),
    }
}

fn load(name: &str) -> Vec<Value> {
    let path = vectors_dir().join(name);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let doc: Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(
        doc["format"], "rbl-link-vectors/1",
        "{name}: unknown format"
    );
    doc["cases"].as_array().expect("cases").clone()
}

fn skip_large() -> bool {
    std::env::var_os("RBL_VECTORS_SKIP_LARGE").is_some_and(|v| v == "1")
}

#[derive(Default)]
struct Report {
    passed: usize,
    skipped: Vec<String>,
    failed: Vec<String>,
    known: Vec<String>,
}

impl Report {
    fn record(&mut self, case: &Value, result: Result<(), String>) {
        let id = case["id"].as_str().unwrap_or("?");
        let basis = case["basis"].as_str().unwrap_or("MUST");
        let section = case["section"].as_str().unwrap_or("");
        let known = KNOWN_DISAGREEMENTS.iter().find(|(k, _)| *k == id);
        match (result, known) {
            (Ok(()), None) => self.passed += 1,
            (Ok(()), Some((_, why))) => self.failed.push(format!(
                "{id}: now passes, remove it from KNOWN_DISAGREEMENTS ({why})"
            )),
            (Err(e), None) => self
                .failed
                .push(format!("{id} [section {section}, {basis}]: {e}")),
            (Err(e), Some((_, why))) => self.known.push(format!("{id}: {e} (known: {why})")),
        }
    }

    fn finish(self, what: &str) {
        println!(
            "{what}: {} passed, {} known disagreements, {} skipped, {} failed",
            self.passed,
            self.known.len(),
            self.skipped.len(),
            self.failed.len()
        );
        for k in &self.known {
            println!("  known disagreement: {k}");
        }
        for s in &self.skipped {
            println!("  skipped: {s}");
        }
        assert!(
            self.failed.is_empty(),
            "{what}: {} vectors failed:\n  {}",
            self.failed.len(),
            self.failed.join("\n  ")
        );
    }
}

fn hex_decode(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "odd hex");
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex"))
        .collect()
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn jf32(v: &Value) -> f32 {
    match v {
        Value::String(s) if s == "inf" => f32::INFINITY,
        other => other
            .as_f64()
            .unwrap_or_else(|| panic!("not a number: {other}")) as f32,
    }
}

fn jf32s<const N: usize>(v: &Value) -> [f32; N] {
    let a = v.as_array().unwrap_or_else(|| panic!("not an array: {v}"));
    assert_eq!(a.len(), N, "array length");
    std::array::from_fn(|i| jf32(&a[i]))
}

fn ju64(v: &Value) -> u64 {
    match v {
        Value::String(s) => s.parse().expect("u64 string"),
        other => other.as_u64().expect("u64"),
    }
}

fn ju32(v: &Value) -> u32 {
    u32::try_from(v.as_u64().unwrap_or_else(|| panic!("not a u32: {v}"))).expect("u32")
}

// ---------------------------------------------------------------------------------------------
// SHA-256, only to check SHA256SUMS

fn sha256(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[4 * i..4 * i + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[test]
fn sha256_matches_known_answers() {
    assert_eq!(
        sha256(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

/// Every file of the vectors directory is what `SHA256SUMS` says, and nothing else is there.
#[test]
fn vectors_match_their_checksums() {
    let dir = vectors_dir();
    let sums = fs::read_to_string(dir.join("SHA256SUMS")).expect("SHA256SUMS");
    let mut listed = Vec::new();
    for line in sums.lines() {
        let (hash, name) = line.split_once("  ").expect("sha256sum line");
        let data = fs::read(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(sha256(&data), hash, "{name} differs from SHA256SUMS");
        listed.push(name.to_owned());
    }
    assert!(listed.len() > 10, "SHA256SUMS lists too little");
}

// ---------------------------------------------------------------------------------------------
// control.json

fn role_of(v: &Value) -> Role {
    Role::parse(v.as_str().expect("role")).expect("role value")
}

fn control_from(m: &Value) -> Control {
    let opt_str = |k: &str| m.get(k).map(|v| v.as_str().expect("string").to_owned());
    let s = |k: &str| m[k].as_str().unwrap_or_else(|| panic!("{k}")).to_owned();
    match m["type"].as_str().expect("type") {
        "hello" => Control::Hello(Hello {
            role: role_of(&m["role"]),
            game: s("game"),
            protocol: ju32(&m["protocol"]) as u16,
            min_protocol: ju32(&m["min_protocol"]) as u16,
            udp_port: ju32(&m["udp_port"]) as u16,
            heartbeat_ms: ju32(&m["heartbeat_ms"]),
            timeout_ms: ju32(&m["timeout_ms"]),
            session: m.get("session").map(ju32),
        }),
        "spawn" => Control::Spawn(Spawn {
            entity: ju32(&m["entity"]),
            provider: s("provider"),
            asset: s("asset"),
            name: opt_str("name"),
        }),
        "despawn" => Control::Despawn {
            entity: ju32(&m["entity"]),
        },
        "input" => Control::Input {
            focus: role_of(&m["focus"]),
            reason: opt_str("reason"),
        },
        "frame_channel" => Control::FrameChannel(FrameChannel {
            path: s("path"),
            width: ju32(&m["width"]),
            height: ju32(&m["height"]),
            slots: ju32(&m["slots"]),
        }),
        "bye" => Control::Bye {
            reason: s("reason"),
        },
        other => panic!("vector message of type {other}"),
    }
}

fn check_control(c: &Value) -> Result<(), String> {
    let line: String = match (c["line"].as_str(), c["line_hex"].as_str()) {
        (Some(l), _) => l.to_owned(),
        (None, Some(h)) => match String::from_utf8(hex_decode(h)) {
            Ok(s) => s,
            // The reader refuses a line that is not UTF-8 before it parses it (sessions.json checks that path).
            Err(_) => return Ok(()),
        },
        _ => panic!("no line"),
    };
    let parsed = Control::parse(&line);
    match c["expect"].as_str().expect("expect") {
        "reject" => match parsed {
            Err(_) => Ok(()),
            Ok(m) => Err(format!("accepted {line:?} as {m:?}")),
        },
        "unknown" => match parsed {
            Ok(Control::Unknown(t)) if t == c["type"].as_str().unwrap() => Ok(()),
            other => Err(format!(
                "{line:?} gave {other:?}, expected the unknown type {}",
                c["type"]
            )),
        },
        "message" => {
            let want = control_from(&c["message"]);
            match parsed {
                Ok(got) if got == want => {}
                other => return Err(format!("{line:?} gave {other:?}, expected {want:?}")),
            }
            if c["encode"] == true {
                let out = want.to_line();
                if !out.ends_with('\n') || out[..out.len() - 1].contains(['\n', '\r']) {
                    return Err(format!("encoded line is not a single line: {out:?}"));
                }
                let v: Value =
                    serde_json::from_str(&out).map_err(|e| format!("encoded {out:?}: {e}"))?;
                if v != c["message"] {
                    return Err(format!(
                        "encoded {out:?}, expected the object {}",
                        c["message"]
                    ));
                }
            }
            Ok(())
        }
        other => panic!("expect {other}"),
    }
}

#[test]
fn control_vectors() {
    let mut r = Report::default();
    for c in load("control.json") {
        let res = check_control(&c);
        r.record(&c, res);
    }
    r.finish("control.json");
}

// ---------------------------------------------------------------------------------------------
// datagrams.json

fn header_of(h: &Value) -> Header {
    Header {
        version: ju32(&h["version"]) as u16,
        flags: h.get("flags").map_or(0, |f| ju32(f) as u8),
        session: ju32(&h["session"]),
        seq: ju32(&h["seq"]),
        time_us: ju64(&h["time_us"]),
    }
}

fn pose_of(p: &Value) -> Pose {
    Pose {
        entity: ju32(&p["entity"]),
        flags: ju32(&p["flags"]),
        time_us: ju64(&p["time_us"]),
        translation: jf32s(&p["translation"]),
        rotation: jf32s(&p["rotation"]),
        scale: jf32s(&p["scale"]),
        velocity: jf32s(&p["velocity"]),
        angular_velocity: jf32s(&p["angular_velocity"]),
    }
}

fn camera_of(b: &Value) -> Camera {
    Camera {
        view: jf32s(&b["view"]),
        proj: jf32s(&b["proj"]),
        fov_y: jf32(&b["fov_y"]),
        near: jf32(&b["near"]),
        far: jf32(&b["far"]),
        viewport: [ju32(&b["viewport_width"]), ju32(&b["viewport_height"])],
        depth_range: match ju32(&b["depth_range"]) {
            0 => DepthRange::NegOneToOne,
            1 => DepthRange::ZeroToOne,
            2 => DepthRange::Reversed,
            n => panic!("depth_range {n}"),
        },
    }
}

fn frame_ready_of(b: &Value) -> FrameReady {
    FrameReady {
        frame: ju64(&b["frame"]),
        slot: ju32(&b["slot"]),
        camera_seq: ju32(&b["camera_seq"]),
        width: ju32(&b["width"]),
        height: ju32(&b["height"]),
    }
}

fn collision_of(b: &Value) -> CollisionChunk {
    CollisionChunk {
        query: ju32(&b["query"]),
        chunk_index: ju32(&b["chunk_index"]) as u16,
        chunk_count: ju32(&b["chunk_count"]) as u16,
        center: jf32s(&b["center"]),
        radius: jf32(&b["radius"]),
        triangles: b["triangles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let f: [f32; 9] = jf32s(t);
                [[f[0], f[1], f[2]], [f[3], f[4], f[5]], [f[6], f[7], f[8]]]
            })
            .collect(),
    }
}

fn datagram_of(b: &Value) -> Datagram {
    match b["kind"].as_str().expect("kind") {
        "heartbeat" => Datagram::Heartbeat,
        "camera" => Datagram::Camera(camera_of(b)),
        "pose" => Datagram::Pose(b["poses"].as_array().unwrap().iter().map(pose_of).collect()),
        "collision" => Datagram::Collision(collision_of(b)),
        "frame" => Datagram::Frame(frame_ready_of(b)),
        other => panic!("body kind {other}"),
    }
}

fn check_datagram(c: &Value) -> Result<(), String> {
    let bytes = hex_decode(c["hex"].as_str().unwrap());
    let got = decode_datagram(&bytes);
    match c["expect"].as_str().unwrap() {
        "reject" => match got {
            Err(_) => Ok(()),
            Ok((h, d)) => Err(format!("accepted ({}): {h:?} {:?}", c["why"], d.kind())),
        },
        "ignore" => match got {
            Ok((_, Datagram::Unknown { kind, payload_len }))
                if u64::from(kind) == c["kind"].as_u64().unwrap()
                    && payload_len as u64 == c["payload_len"].as_u64().unwrap() =>
            {
                Ok(())
            }
            other => Err(format!(
                "expected an unknown kind {}, got {other:?}",
                c["kind"]
            )),
        },
        "decode" => {
            let (h, body) = got.map_err(|e| format!("rejected: {e}"))?;
            let want_h = header_of(&c["header"]);
            let flags_listed = c["header"].get("flags").is_some();
            if (h.version, h.session, h.seq, h.time_us)
                != (want_h.version, want_h.session, want_h.seq, want_h.time_us)
                || (flags_listed && h.flags != want_h.flags)
            {
                return Err(format!("header {h:?}, expected {want_h:?}"));
            }
            let want = datagram_of(&c["body"]);
            if body != want {
                return Err(format!("body {body:?}, expected {want:?}"));
            }
            if c["encode"] == true {
                let mut out = Vec::new();
                encode_datagram(&want_h, &want, &mut out)
                    .map_err(|e| format!("encode failed: {e}"))?;
                if out != bytes {
                    return Err(format!(
                        "encoded {}, expected {}",
                        hex_encode(&out),
                        hex_encode(&bytes)
                    ));
                }
            }
            Ok(())
        }
        other => panic!("expect {other}"),
    }
}

#[test]
fn datagram_vectors() {
    let mut r = Report::default();
    for c in load("datagrams.json") {
        let res = check_datagram(&c);
        r.record(&c, res);
    }
    r.finish("datagrams.json");
}

// ---------------------------------------------------------------------------------------------
// pixelfiles.json

struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Self {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("rbl-vectors-{}-{tag}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn copy_fixture(case: &Value, to: &Path) {
    let from = vectors_dir().join(case["file"].as_str().expect("file"));
    fs::copy(from, to).unwrap();
}

fn make_recipe(recipe: &Value, to: &Path) {
    let header = hex_decode(recipe["header_hex"].as_str().unwrap());
    let f = fs::File::create(to).unwrap();
    f.set_len(recipe["length"].as_u64().unwrap()).unwrap();
    drop(f);
    let mut f = fs::OpenOptions::new().write(true).open(to).unwrap();
    f.write_all(&header).unwrap();
}

fn check_read(reader: &FrameReader, rd: &Value) -> Result<(), String> {
    let ready = frame_ready_of(rd);
    let want_depth = rd["want_depth"] == true;
    let mut rgba = Vec::new();
    let mut depth = Vec::new();
    let got = if want_depth {
        reader.read_depth(&ready, &mut rgba, &mut depth)
    } else {
        reader.read(&ready, &mut rgba)
    };
    match rd["expect"].as_str().unwrap() {
        "skip" => match got {
            Err(_) => Ok(()),
            Ok(d) => Err(format!("delivered {d:?} ({})", rd["why"])),
        },
        "ok" => {
            let d = got
                .map_err(|m| format!("frame {} slot {} skipped: {m:?}", rd["frame"], rd["slot"]))?;
            let mut want = vec![
                ("frame", ju64(&rd["frame"]), d.frame),
                ("time_us", ju64(&rd["time_us"]), d.time_us),
            ];
            want.push((
                "camera_seq",
                u64::from(ju32(&rd["camera_seq"])),
                u64::from(d.camera_seq),
            ));
            want.push(("width", u64::from(ju32(&rd["width"])), u64::from(d.width)));
            want.push((
                "height",
                u64::from(ju32(&rd["height"])),
                u64::from(d.height),
            ));
            for (name, w, g) in want {
                if w != g {
                    return Err(format!("{name} is {g}, expected {w}"));
                }
            }
            match rd["rgba"].as_str() {
                Some(h) if hex_encode(&rgba) != h => return Err("colour bytes differ".into()),
                Some(_) => {}
                None => {
                    if sha256(&rgba) != rd["rgba_sha256"].as_str().unwrap() {
                        return Err("colour bytes differ (sha256)".into());
                    }
                }
            }
            match rd["depth"].as_array() {
                None => {
                    if !depth.is_empty() {
                        return Err(format!("{} depth values, expected none", depth.len()));
                    }
                }
                Some(a) => {
                    let want: Vec<f32> = a.iter().map(jf32).collect();
                    if depth.len() != want.len()
                        || depth
                            .iter()
                            .zip(&want)
                            .any(|(g, w)| g.to_bits() != w.to_bits())
                    {
                        return Err(format!("depth {depth:?}, expected {want:?}"));
                    }
                }
            }
            Ok(())
        }
        other => panic!("read expect {other}"),
    }
}

fn check_pixel_reader(c: &Value) -> Result<(), String> {
    let t = Temp::new("pf");
    let path = t.path("pixel.frames");
    let open_path: PathBuf = match c["path_kind"].as_str() {
        None => {
            if c.get("recipe").is_some() {
                make_recipe(&c["recipe"], &path);
            } else {
                copy_fixture(c, &path);
            }
            path.clone()
        }
        Some("relative") => {
            // A valid copy sits in the working directory; only the relative spelling may refuse it.
            let name = format!("rbl-vector-relative-{}.frames", std::process::id());
            copy_fixture(c, Path::new(&name));
            let r = FrameReader::open(Path::new(&name));
            let _ = fs::remove_file(&name);
            return match r {
                Err(_) => Ok(()),
                Ok(_) => Err("opened a relative path".into()),
            };
        }
        Some("missing") => t.path("does-not-exist.frames"),
        Some("empty") => PathBuf::new(),
        Some("directory") => {
            let d = t.path("a-directory");
            fs::create_dir(&d).unwrap();
            d
        }
        Some("symlink") => {
            copy_fixture(c, &path);
            #[cfg(unix)]
            {
                let link = t.path("link.frames");
                std::os::unix::fs::symlink(&path, &link).unwrap();
                link
            }
            #[cfg(not(unix))]
            return Ok(());
        }
        Some("fifo") => {
            #[cfg(unix)]
            {
                let fifo = t.path("fifo.frames");
                let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
                // SAFETY: a valid NUL-terminated path.
                assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo");
                fifo
            }
            #[cfg(not(unix))]
            return Ok(());
        }
        Some(other) => panic!("path_kind {other}"),
    };
    let opened = FrameReader::open(&open_path);
    match c["open"].as_str().unwrap() {
        "refused" => match opened {
            Err(_) => Ok(()),
            Ok(_) => Err(format!("opened the file ({})", c["why"])),
        },
        "ok" => {
            let reader = opened.map_err(|e| format!("refused: {e}"))?;
            if let Some(info) = c.get("info") {
                if reader.has_depth() != (info["has_depth"] == true) {
                    return Err(format!(
                        "has_depth is {}, expected {}",
                        reader.has_depth(),
                        info["has_depth"]
                    ));
                }
            }
            for rd in c
                .get("reads")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                check_read(&reader, rd).map_err(|e| {
                    format!("read of frame {} slot {}: {e}", rd["frame"], rd["slot"])
                })?;
            }
            for step in c
                .get("then")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                match step["op"].as_str().unwrap() {
                    "truncate" => {
                        let f = fs::OpenOptions::new().write(true).open(&path).unwrap();
                        f.set_len(step["length"].as_u64().unwrap()).unwrap();
                    }
                    "read" => {
                        check_read(&reader, step).map_err(|e| format!("after truncation: {e}"))?
                    }
                    other => panic!("then op {other}"),
                }
            }
            Ok(())
        }
        other => panic!("open {other}"),
    }
}

fn check_pixel_writer(c: &Value) -> Result<(), String> {
    let w = &c["writer"];
    let t = Temp::new("pw");
    let path = t.path("writer.frames");
    let opts = {
        let o = FrameFileOptions::new(ju32(&w["width"]), ju32(&w["height"]), ju32(&w["slots"]));
        if w["with_depth"] == true {
            o.with_depth()
        } else {
            o
        }
    };
    match c.get("expect").and_then(Value::as_str) {
        Some("create_fails_and_file_untouched") => {
            fs::write(&path, b"precious").unwrap();
            if FrameWriter::create_with(&path, &opts).is_ok() {
                return Err("created a writer over an existing file".into());
            }
            return if fs::read(&path).unwrap() == b"precious" {
                Ok(())
            } else {
                Err("the existing file was changed".into())
            };
        }
        Some("create_fails") => {
            return match FrameWriter::create_with(&path, &opts) {
                Err(_) if !path.exists() => Ok(()),
                Err(_) => Err("failed but left a file".into()),
                Ok(_) => Err(format!("created a writer ({})", c["why"])),
            };
        }
        _ => {}
    }
    let mut writer =
        FrameWriter::create_with(&path, &opts).map_err(|e| format!("create failed: {e}"))?;
    for (i, wr) in w["writes"].as_array().unwrap().iter().enumerate() {
        let rgba = hex_decode(wr["rgba"].as_str().unwrap());
        let (width, height) = (ju32(&wr["width"]), ju32(&wr["height"]));
        let depth: Option<Vec<f32>> = wr["depth"].as_array().map(|a| a.iter().map(jf32).collect());
        let ready = writer
            .write_depth(
                &rgba,
                depth.as_deref(),
                width,
                height,
                ju32(&wr["camera_seq"]),
                ju64(&wr["time_us"]),
            )
            .map_err(|e| format!("write {i} failed: {e}"))?;
        if ready.frame != i as u64 + 1
            || ready.slot != ((i as u64 + 1) % u64::from(opts.slots)) as u32
        {
            return Err(format!("write {i} announced {ready:?}"));
        }
    }
    let got = fs::read(&path).unwrap();
    let want = fs::read(vectors_dir().join(c["file"].as_str().unwrap())).unwrap();
    if got != want {
        let at = got.iter().zip(&want).position(|(a, b)| a != b);
        return Err(format!(
            "file bytes differ (lengths {} and {}, first difference at {at:?})",
            got.len(),
            want.len()
        ));
    }
    Ok(())
}

#[test]
fn pixel_file_vectors() {
    let mut r = Report::default();
    for c in load("pixelfiles.json") {
        if c.get("writer").is_some() {
            continue;
        }
        if c["large"] == true && skip_large() {
            r.skipped.push(format!("{} (large)", c["id"]));
            continue;
        }
        if cfg!(windows)
            && c["skip_on"]
                .as_array()
                .is_some_and(|a| a.iter().any(|p| p == "windows"))
        {
            r.skipped.push(format!("{} (windows)", c["id"]));
            continue;
        }
        let res = check_pixel_reader(&c);
        r.record(&c, res);
    }
    r.finish("pixelfiles.json (readers)");
}

#[test]
fn pixel_writer_vectors() {
    let mut r = Report::default();
    for c in load("pixelfiles.json") {
        if c.get("writer").is_some() {
            let res = check_pixel_writer(&c);
            r.record(&c, res);
        }
    }
    r.finish("pixelfiles.json (writers)");
}

// ---------------------------------------------------------------------------------------------
// sessions.json

const STEP_WAIT: Duration = Duration::from_millis(3000);
const LO: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0);

struct Raw {
    stream: TcpStream,
    buf: Vec<u8>,
    eof: bool,
}

struct Session {
    ut: Link,
    events: Vec<(String, Value)>,
    conns: HashMap<String, Raw>,
    listener: Option<TcpListener>,
    udp: UdpSocket,
    other: Option<UdpSocket>,
    vars: HashMap<String, String>,
    ut_udp: u16,
    datagrams: HashMap<String, Value>,
}

fn event_json(e: Event) -> (String, Value) {
    use serde_json::json;
    match e {
        Event::Connected(p) => (
            "connected".into(),
            json!({"role": p.role.as_str(), "game": p.game, "protocol": p.protocol, "session": p.session,
                   "heartbeat_ms": p.heartbeat.as_millis() as u64}),
        ),
        Event::Lost { .. } => ("lost".into(), json!({})),
        Event::HandshakeFailed { .. } => ("handshake_failed".into(), json!({})),
        Event::Camera { header, camera } => (
            "camera".into(),
            json!({"seq": header.seq, "viewport_width": camera.viewport[0], "viewport_height": camera.viewport[1],
                   "depth_range": camera.depth_range as u32}),
        ),
        Event::Poses { poses, .. } => (
            "poses".into(),
            json!({"count": poses.len(), "entities": poses.iter().map(|p| p.entity).collect::<Vec<_>>()}),
        ),
        Event::Collision(c) => (
            "collision".into(),
            json!({"query": c.query, "triangles": c.triangles.len()}),
        ),
        Event::Frame { frame, .. } => (
            "frame".into(),
            json!({"frame": frame.frame.to_string(), "slot": frame.slot, "camera_seq": frame.camera_seq,
                   "width": frame.width, "height": frame.height}),
        ),
        Event::Spawn(s) => {
            let mut v = json!({"entity": s.entity, "provider": s.provider, "asset": s.asset});
            if let Some(n) = s.name {
                v["name"] = n.into();
            }
            ("spawn".into(), v)
        }
        Event::Despawn { entity } => ("despawn".into(), json!({"entity": entity})),
        Event::Focus { focus, requested } => (
            "focus".into(),
            json!({"focus": focus.as_str(), "requested": requested}),
        ),
        Event::FrameChannel(c) => (
            "frame_channel".into(),
            json!({"path": c.path, "width": c.width, "height": c.height, "slots": c.slots}),
        ),
    }
}

/// `pattern` is a literal, or a string `$name` pattern.
fn pattern_ok(actual: &Value, pattern: &Value) -> bool {
    if let Some(p) = pattern.as_str().filter(|p| p.starts_with('$')) {
        let int = actual.as_u64();
        return match p {
            "$string" => actual.is_string(),
            "$int" => int.is_some(),
            "$u32nz" => int.is_some_and(|n| (1..=u64::from(u32::MAX)).contains(&n)),
            "$u16nz" => int.is_some_and(|n| (1..=65535).contains(&n)),
            "$hb" => int.is_some_and(|n| (10..=10_000).contains(&n)),
            other => panic!("pattern {other}"),
        };
    }
    actual == pattern
}

fn match_ok(actual: &Value, m: &Value) -> Result<(), String> {
    for (k, want) in m.as_object().unwrap() {
        match actual.get(k) {
            Some(got) if pattern_ok(got, want) => {}
            got => {
                return Err(format!(
                    "key {k:?}: expected {want}, got {got:?} in {actual}"
                ))
            }
        }
    }
    Ok(())
}

impl Session {
    fn new(c: &Value, all: &HashMap<String, Value>) -> Self {
        let cfgj = c.get("config").cloned().unwrap_or(Value::Null);
        let ms = |k: &str, d: u64| {
            Duration::from_millis(cfgj.get(k).and_then(Value::as_u64).unwrap_or(d))
        };
        let udp = UdpSocket::bind(LO).unwrap();
        udp.set_nonblocking(true).unwrap();
        let host = c["role_under_test"] == "host";
        let (listener, port) = if host {
            (None, 0)
        } else {
            let l = TcpListener::bind(LO).unwrap();
            l.set_nonblocking(true).unwrap();
            let p = l.local_addr().unwrap().port();
            (Some(l), p)
        };
        let cfg = LinkConfig {
            control_port: port,
            heartbeat: ms("heartbeat_ms", 250),
            timeout: ms("timeout_ms", 2000),
            reconnect: ms("reconnect_ms", 200),
            ..LinkConfig::new("vector ut")
        };
        let ut = if host {
            Link::host(cfg).unwrap()
        } else {
            Link::guest(cfg).unwrap()
        };
        let ut_udp = ut.udp_addr().unwrap().port();
        let mut vars = HashMap::new();
        vars.insert(
            "peer_udp_port".to_owned(),
            udp.local_addr().unwrap().port().to_string(),
        );
        vars.insert("ut_udp_port".to_owned(), ut_udp.to_string());
        Self {
            ut,
            events: vec![],
            conns: HashMap::new(),
            listener,
            udp,
            other: None,
            vars,
            ut_udp,
            datagrams: all.clone(),
        }
    }

    fn pump(&mut self) {
        for e in self.ut.poll() {
            self.events.push(event_json(e));
        }
    }

    /// Polls the UT until `f` returns something, for at most `limit`.
    fn wait<T>(&mut self, limit: Duration, mut f: impl FnMut(&mut Self) -> Option<T>) -> Option<T> {
        let end = Instant::now() + limit;
        loop {
            self.pump();
            if let Some(v) = f(self) {
                return Some(v);
            }
            if Instant::now() >= end {
                return None;
            }
            sleep(Duration::from_millis(2));
        }
    }

    fn subst(&self, s: &str) -> String {
        let mut out = s.to_owned();
        for (k, v) in &self.vars {
            out = out.replace(&format!("${{{k}}}"), v);
        }
        out
    }

    fn session_var(&self) -> u32 {
        self.vars
            .get("session")
            .and_then(|s| s.parse().ok())
            .expect("a session variable is needed here")
    }

    fn subst_hex(&self, s: &str) -> String {
        let mut out = s.to_owned();
        if out.contains("{session_le") {
            let session = self.session_var();
            for (tag, v) in [
                ("{session_le}", session),
                ("{session_le_plus1}", session.wrapping_add(1)),
                ("{session_le_minus1}", session.wrapping_sub(1)),
            ] {
                out = out.replace(tag, &hex_encode(&v.to_le_bytes()));
            }
        }
        out
    }

    fn conn(&mut self, step: &Value) -> &mut Raw {
        let name = step
            .get("conn")
            .and_then(Value::as_str)
            .unwrap_or("a")
            .to_owned();
        self.conns
            .get_mut(&name)
            .unwrap_or_else(|| panic!("no connection {name}"))
    }

    /// Reads what is available into the connection's buffer.
    fn fill(raw: &mut Raw) {
        let mut tmp = [0u8; 16384];
        loop {
            match raw.stream.read(&mut tmp) {
                Ok(0) => {
                    raw.eof = true;
                    return;
                }
                Ok(n) => raw.buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => {
                    raw.eof = true;
                    return;
                }
            }
        }
    }

    fn take_line(raw: &mut Raw) -> Option<Vec<u8>> {
        let i = raw.buf.iter().position(|&b| b == b'\n')?;
        let line: Vec<u8> = raw.buf.drain(..=i).collect();
        Some(line)
    }

    fn write_all(&mut self, step: &Value, data: &[u8]) {
        let name = step
            .get("conn")
            .and_then(Value::as_str)
            .unwrap_or("a")
            .to_owned();
        // The stream is non-blocking so that reads never stall: write in a loop, polling the UT meanwhile.
        let mut sent = 0;
        let end = Instant::now() + Duration::from_secs(10);
        while sent < data.len() {
            match self
                .conns
                .get_mut(&name)
                .unwrap()
                .stream
                .write(&data[sent..])
            {
                Ok(n) => sent += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    self.pump();
                    sleep(Duration::from_millis(1));
                }
                Err(e) => panic!("write failed: {e}"),
            }
            assert!(Instant::now() < end, "write timed out");
        }
    }

    fn run(&mut self, steps: &[Value]) -> Result<(), String> {
        for (n, step) in steps.iter().enumerate() {
            let op = step["op"].as_str().unwrap();
            self.step(op, step)
                .map_err(|e| format!("step {n} ({op}): {e}"))?;
        }
        Ok(())
    }

    fn step(&mut self, op: &str, step: &Value) -> Result<(), String> {
        let name = step
            .get("conn")
            .and_then(Value::as_str)
            .unwrap_or("a")
            .to_owned();
        match op {
            "connect" => {
                let port = self.ut.control_addr().expect("host").port();
                let s = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
                s.set_nodelay(true).unwrap();
                s.set_nonblocking(true).unwrap();
                self.conns.insert(
                    name,
                    Raw {
                        stream: s,
                        buf: vec![],
                        eof: false,
                    },
                );
            }
            "accept" => {
                let got = self.wait(STEP_WAIT, |me| {
                    match me.listener.as_ref().unwrap().accept() {
                        Ok((s, _)) => Some(s),
                        Err(_) => None,
                    }
                });
                let s = got.ok_or("the guest did not connect")?;
                s.set_nodelay(true).unwrap();
                s.set_nonblocking(true).unwrap();
                self.conns.insert(
                    name,
                    Raw {
                        stream: s,
                        buf: vec![],
                        eof: false,
                    },
                );
            }
            "send_line" => {
                let text = self.subst(step["text"].as_str().unwrap());
                self.write_all(step, text.as_bytes());
            }
            "send_hex" => {
                let b = hex_decode(step["hex"].as_str().unwrap());
                self.write_all(step, &b);
            }
            "send_fill" => {
                let mut b = step["prefix"].as_str().unwrap().as_bytes().to_vec();
                b.extend(std::iter::repeat_n(
                    step["fill"].as_str().unwrap().as_bytes()[0],
                    step["count"].as_u64().unwrap() as usize,
                ));
                b.extend_from_slice(step["suffix"].as_str().unwrap().as_bytes());
                self.write_all(step, &b);
            }
            "send_range" => {
                let t = step["template"].as_str().unwrap();
                let mut b = Vec::new();
                for i in step["from"].as_u64().unwrap()..=step["to"].as_u64().unwrap() {
                    b.extend_from_slice(self.subst(&t.replace("${i}", &i.to_string())).as_bytes());
                }
                self.write_all(step, &b);
            }
            "expect_line" => {
                let line = self
                    .wait(STEP_WAIT, |me| {
                        let raw = me.conn(step);
                        Self::fill(raw);
                        Self::take_line(raw)
                    })
                    .ok_or("no line from the implementation")?;
                let text = String::from_utf8_lossy(&line).into_owned();
                let v: Value = serde_json::from_str(text.trim_end())
                    .map_err(|e| format!("not JSON: {text:?}: {e}"))?;
                match_ok(&v, &step["match"]).map_err(|e| format!("line {text:?}: {e}"))?;
                if step["exact"] == true {
                    let mut got: Vec<&String> =
                        v.as_object().ok_or("not an object")?.keys().collect();
                    let mut want: Vec<&String> =
                        step["match"].as_object().unwrap().keys().collect();
                    got.sort();
                    want.sort();
                    if got != want {
                        return Err(format!("keys {got:?}, expected exactly {want:?}"));
                    }
                }
                if let Some(cap) = step.get("capture").and_then(Value::as_object) {
                    for (var, key) in cap {
                        let val = v[key.as_str().unwrap()].clone();
                        let s = val
                            .as_u64()
                            .map(|n| n.to_string())
                            .ok_or("captured value is not an integer")?;
                        if var == "ut_udp_port" && s != self.ut_udp.to_string() {
                            return Err(format!(
                                "hello announces udp_port {s}, the socket is {}",
                                self.ut_udp
                            ));
                        }
                        self.vars.insert(var.clone(), s);
                    }
                }
            }
            "expect_no_line" => {
                let ms = step["ms"].as_u64().unwrap();
                let bad = self.wait(Duration::from_millis(ms), |me| {
                    let raw = me.conn(step);
                    Self::fill(raw);
                    if let Some(l) = Self::take_line(raw) {
                        return Some(format!("a line arrived: {:?}", String::from_utf8_lossy(&l)));
                    }
                    raw.eof.then(|| "the connection closed".to_owned())
                });
                if let Some(b) = bad {
                    return Err(b);
                }
            }
            "expect_closed" => {
                let allow = step["allow_lines"] == true;
                let within = Duration::from_millis(
                    step.get("within_ms")
                        .and_then(Value::as_u64)
                        .unwrap_or(3000),
                );
                let r = self.wait(within, |me| {
                    let raw = me.conn(step);
                    Self::fill(raw);
                    if !allow {
                        if let Some(l) = Self::take_line(raw) {
                            return Some(Err(format!(
                                "a line arrived before end of stream: {:?}",
                                String::from_utf8_lossy(&l)
                            )));
                        }
                    }
                    raw.eof.then_some(Ok(()))
                });
                match r {
                    Some(Ok(())) => {}
                    Some(Err(e)) => return Err(e),
                    None => return Err("the connection stayed open".into()),
                }
            }
            "close" => {
                let raw = self.conns.get_mut(&name).unwrap();
                let how = if step["half"] == true {
                    std::net::Shutdown::Write
                } else {
                    std::net::Shutdown::Both
                };
                let _ = raw.stream.shutdown(how);
                if step["half"] != true {
                    self.conns.remove(&name);
                }
            }
            "set" => {
                for (k, v) in step["vars"].as_object().unwrap() {
                    self.vars.insert(
                        k.clone(),
                        v.as_u64()
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| v.as_str().unwrap().to_owned()),
                    );
                }
            }
            "sleep" => {
                self.wait(Duration::from_millis(step["ms"].as_u64().unwrap()), |_| {
                    None::<()>
                });
            }
            "send_datagram" => {
                let b = hex_decode(&self.subst_hex(step["hex"].as_str().unwrap()));
                let dest = SocketAddr::new(LO.ip(), self.ut_udp);
                if step["from"] == "other" {
                    let o = self
                        .other
                        .get_or_insert_with(|| UdpSocket::bind(LO).unwrap());
                    o.send_to(&b, dest).map_err(|e| e.to_string())?;
                } else {
                    self.udp.send_to(&b, dest).map_err(|e| e.to_string())?;
                }
            }
            "expect_datagram" => self.expect_datagram(step)?,
            "expect_no_datagram" => {
                let ms = step["ms"].as_u64().unwrap();
                let bad = self.wait(Duration::from_millis(ms), |me| {
                    let mut buf = [0u8; 16384];
                    me.udp
                        .recv_from(&mut buf)
                        .ok()
                        .map(|(n, from)| format!("a {n} byte datagram arrived from {from}"))
                });
                if let Some(b) = bad {
                    return Err(b);
                }
            }
            "expect_focus" => {
                self.pump();
                let got = self.ut.focus().as_str();
                if got != step["focus"].as_str().unwrap() {
                    return Err(format!("focus is {got}, expected {}", step["focus"]));
                }
            }
            "ut" => self.ut_do(step)?,
            "expect_event" => {
                let ty = step["event"].as_str().unwrap().to_owned();
                let count = step.get("count").and_then(Value::as_u64).unwrap_or(1) as usize;
                let ok = self
                    .wait(STEP_WAIT, |me| {
                        (me.events.iter().filter(|(t, _)| *t == ty).count() >= count).then_some(())
                    })
                    .is_some();
                if !ok {
                    let have: Vec<&str> = self.events.iter().map(|(t, _)| t.as_str()).collect();
                    return Err(format!("no {count} {ty} event(s); queue holds {have:?}"));
                }
                let mut taken = 0;
                let mut i = 0;
                while taken < count {
                    if self.events[i].0 == ty {
                        let (_, v) = self.events.remove(i);
                        if let Some(m) = step.get("match") {
                            match_ok(&v, m).map_err(|e| format!("{ty} event {taken}: {e}"))?;
                        }
                        taken += 1;
                    } else {
                        i += 1;
                    }
                }
            }
            "expect_no_event" => {
                let ms = step["ms"].as_u64().unwrap();
                self.wait(Duration::from_millis(ms), |_| None::<()>);
                let ty = step["event"].as_str().unwrap();
                if let Some((_, v)) = self.events.iter().find(|(t, _)| t == ty) {
                    return Err(format!("unexpected {ty} event {v}"));
                }
            }
            other => panic!("unknown op {other}"),
        }
        Ok(())
    }

    fn expect_datagram(&mut self, step: &Value) -> Result<(), String> {
        let session = self.session_var();
        let timeout = Duration::from_millis(
            step.get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(3000),
        );
        let count = step.get("count").and_then(Value::as_u64).unwrap_or(1) as usize;
        let like: Option<Vec<u8>> = step.get("like").and_then(Value::as_str).map(|id| {
            hex_decode(
                self.datagrams[id]["hex"]
                    .as_str()
                    .unwrap_or_else(|| panic!("no datagram case {id}")),
            )
        });
        let kind: Option<u8> = match (step.get("kind").and_then(Value::as_u64), &like) {
            (Some(k), _) => Some(k as u8),
            (None, Some(l)) => Some(l[6]),
            _ => None,
        };
        let mut seqs = Vec::new();
        let ut_udp = self.ut_udp;
        let r: Option<Result<(), String>> = self.wait(timeout, |me| {
            let mut buf = [0u8; 16384];
            loop {
                let (n, from) = match me.udp.recv_from(&mut buf) {
                    Ok(r) => r,
                    Err(_) => return None,
                };
                let d = &buf[..n];
                if from.port() != ut_udp {
                    return Some(Err(format!(
                        "datagram from {from}, the UT's port is {ut_udp}"
                    )));
                }
                if n < 24 || d[..4] != *b"RBLK" || u16::from_le_bytes([d[4], d[5]]) != 1 {
                    return Some(Err(format!(
                        "bad header in {}",
                        hex_encode(&d[..n.min(24)])
                    )));
                }
                if u32::from_le_bytes(d[8..12].try_into().unwrap()) != session {
                    return Some(Err("wrong session in a datagram from the UT".into()));
                }
                if kind.is_some_and(|k| d[6] != k) {
                    continue;
                }
                if let Some(l) = &like {
                    let same = n == l.len() && d[..8] == l[..8] && d[24..] == l[24..];
                    if !same {
                        return Some(Err(format!(
                            "datagram {} differs from {}",
                            hex_encode(d),
                            hex_encode(l)
                        )));
                    }
                }
                seqs.push(u32::from_le_bytes(d[12..16].try_into().unwrap()));
                if seqs.len() >= count {
                    return Some(Ok(()));
                }
            }
        });
        match r {
            Some(Ok(())) => {}
            Some(Err(e)) => return Err(e),
            None => return Err(format!("got {} of {count} datagrams", seqs.len())),
        }
        if step["seq_consecutive"] == true && seqs.windows(2).any(|w| w[1] != w[0].wrapping_add(1))
        {
            return Err(format!("seq values {seqs:?} are not consecutive"));
        }
        Ok(())
    }

    fn ut_do(&mut self, step: &Value) -> Result<(), String> {
        let what = step["do"].as_str().unwrap();
        let like_body =
            |me: &Self| -> Value { me.datagrams[step["like"].as_str().unwrap()]["body"].clone() };
        let e = |r: Result<(), LinkError>| r.map_err(|e| e.to_string());
        match what {
            "spawn" => e(self.ut.spawn(&Spawn {
                entity: ju32(&step["entity"]),
                provider: step["provider"].as_str().unwrap().to_owned(),
                asset: step["asset"].as_str().unwrap().to_owned(),
                name: step["name"].as_str().map(str::to_owned),
            })),
            "despawn" => e(self.ut.despawn(ju32(&step["entity"]))),
            "set_focus" => e(self.ut.set_focus(role_of(&step["focus"]))),
            "announce_frames" => e(self.ut.announce_frames(&FrameChannel {
                path: step["path"].as_str().unwrap().to_owned(),
                width: ju32(&step["width"]),
                height: ju32(&step["height"]),
                slots: ju32(&step["slots"]),
            })),
            "disconnect" => {
                self.ut.disconnect(step["reason"].as_str().unwrap());
                Ok(())
            }
            "send_camera" => match datagram_of(&like_body(self)) {
                Datagram::Camera(c) => self
                    .ut
                    .send_camera(&c)
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
                _ => panic!("not a camera"),
            },
            "send_poses" => match datagram_of(&like_body(self)) {
                Datagram::Pose(p) => e(self.ut.send_poses(&p)),
                _ => panic!("not poses"),
            },
            "send_frame" => match datagram_of(&like_body(self)) {
                Datagram::Frame(f) => e(self.ut.send_frame(&f)),
                _ => panic!("not a frame"),
            },
            other => panic!("ut do {other}"),
        }
    }
}

#[test]
fn session_vectors() {
    let all: HashMap<String, Value> = load("datagrams.json")
        .into_iter()
        .map(|c| (c["id"].as_str().unwrap().to_owned(), c))
        .collect();
    let mut r = Report::default();
    for c in load("sessions.json") {
        let mut s = Session::new(&c, &all);
        let res = s.run(c["steps"].as_array().unwrap());
        drop(s);
        r.record(&c, res);
    }
    r.finish("sessions.json");
}
