//! Revision 6 of the contract (section 6.2): the token of a guest's `hello`, its constant-time comparison,
//! its generation, the endpoint file, and that no token ever shows up in an error, an event, a `bye` or a
//! `Debug` string. The shared vectors (tests/link_vectors.rs) cover the wire; these cover the rest.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use rewrite_bridge::link::*;

/// A fresh, private directory, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rbl-token-{}-{tag}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
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

#[cfg(unix)]
fn mode_of(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

fn cfg(game: &str) -> LinkConfig {
    LinkConfig {
        control_port: 0,
        heartbeat: Duration::from_millis(50),
        timeout: Duration::from_millis(2000),
        reconnect: Duration::from_millis(50),
        ..LinkConfig::new(game)
    }
}

/// Polls both links until `done` holds on the events seen so far, or 5 seconds pass.
fn run_until(
    a: &mut Link,
    b: &mut Link,
    mut done: impl FnMut(&[Event], &[Event]) -> bool,
) -> (Vec<Event>, Vec<Event>) {
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end && !done(&ea, &eb) {
        ea.extend(a.poll());
        eb.extend(b.poll());
        sleep(Duration::from_millis(2));
    }
    (ea, eb)
}

fn connected(e: &[Event]) -> bool {
    e.iter().any(|e| matches!(e, Event::Connected(_)))
}

fn failed(e: &[Event]) -> Option<&str> {
    e.iter().find_map(|e| match e {
        Event::HandshakeFailed { reason } => Some(reason.as_str()),
        _ => None,
    })
}

const T: &str = "5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e";
const W: &str = "5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a9e5c1f0a90";

// ---------------------------------------------------------------------------------------------
// Comparison and generation

#[test]
fn token_equal_compares_lengths_and_every_byte() {
    let t = T.as_bytes();
    assert!(token_equal(t, t));
    assert!(token_equal(b"a", b"a"));
    let mut first = t.to_vec();
    first[0] ^= 1;
    assert!(!token_equal(t, &first), "differs at the first byte");
    let mut last = t.to_vec();
    *last.last_mut().unwrap() ^= 1;
    assert!(!token_equal(t, &last), "differs at the last byte");
    assert!(!token_equal(t, &t[..63]), "a prefix");
    assert!(!token_equal(&t[..63], t), "a prefix, the other way round");
    assert!(!token_equal(t, format!("{T}0").as_bytes()), "longer");
    assert!(!token_equal(b"", t));
    // The empty token is equal to itself as far as the function goes; the handshake never gets there,
    // since it refuses a token outside 1 ..= 256 bytes first.
    assert!(token_equal(b"", b""));
    // A zero byte past the end of the shorter token is not a match: the lengths are compared too.
    assert!(!token_equal(b"ab", b"ab\0"));
    let a256 = vec![b't'; 256];
    assert!(token_equal(&a256, &a256.clone()));
    let mut b256 = a256.clone();
    b256[255] = b'u';
    assert!(!token_equal(&a256, &b256));
    let a257 = vec![b't'; 257];
    assert!(
        !token_equal(&a257, &a257.clone()),
        "longer than 256 bytes is never equal"
    );
    assert!(
        !token_equal(&a256, &a257),
        "a 256-byte token and the same followed by one more byte"
    );
}

#[test]
fn generated_tokens_are_64_lowercase_hex_digits_and_differ() {
    let a = generate_token().unwrap();
    let b = generate_token().unwrap();
    for t in [&a, &b] {
        assert_eq!(t.len(), 64);
        assert!(t
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
    }
    assert_ne!(a, b);
}

// ---------------------------------------------------------------------------------------------
// The endpoint file

#[test]
fn endpoint_file_round_trip_is_owner_only_and_atomic() {
    let d = Dir::new("roundtrip");
    let path = d.path("host.json");
    let e = Endpoint {
        control_port: 47800,
        token: T.into(),
    };
    e.write(&path).unwrap();
    #[cfg(unix)]
    assert_eq!(mode_of(&path), 0o600);
    assert_eq!(Endpoint::read(&path).unwrap(), e);
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(
        text,
        format!("{{\"control_port\":47800,\"token\":\"{T}\"}}\n")
    );
    // Replacing an existing file, and leaving no temporary file behind.
    let e2 = Endpoint {
        control_port: 1,
        token: "é".repeat(128),
    };
    e2.write(&path).unwrap();
    assert_eq!(Endpoint::read(&path).unwrap(), e2);
    assert_eq!(d.names(), vec!["host.json".to_owned()]);
    Endpoint::remove(&path).unwrap();
    assert!(d.names().is_empty());
}

#[test]
fn endpoint_write_refuses_what_a_reader_would() {
    let d = Dir::new("write-refuses");
    let path = d.path("e.json");
    for (port, token) in [
        (0, T.to_owned()),
        (1, String::new()),
        (1, "t".repeat(257)),
        (1, "a\0b".into()),
    ] {
        let err = Endpoint {
            control_port: port,
            token: token.clone(),
        }
        .write(&path)
        .unwrap_err();
        if !token.is_empty() {
            assert!(!err.to_string().contains(&token));
        }
    }
    assert!(d.names().is_empty(), "nothing written");
    let missing_dir = d.path("no-such-dir").join("e.json");
    assert!(Endpoint {
        control_port: 1,
        token: T.into()
    }
    .write(&missing_dir)
    .is_err());
}

#[cfg(unix)]
#[test]
fn endpoint_read_refuses_a_file_others_can_read_and_never_quotes_it() {
    use std::os::unix::fs::PermissionsExt;
    let d = Dir::new("read-refuses");
    let path = d.path("e.json");
    Endpoint {
        control_port: 47800,
        token: T.into(),
    }
    .write(&path)
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let err = Endpoint::read(&path).unwrap_err();
    assert!(!err.to_string().contains(T));
    // Broken contents that hold the token: the error says what is wrong, not what is there.
    for text in [
        format!("{{\"control_port\":0,\"token\":\"{T}\"}}"),
        format!("{{\"control_port\":1,\"token\":\"{T}\",}}"),
        format!("{{\"control_port\":1,\"token\":\"{}\"}}", T.repeat(5)),
        format!("[\"{T}\"]"),
        T.to_owned(),
    ] {
        fs::write(&path, &text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let err = Endpoint::read(&path).unwrap_err();
        assert!(!err.to_string().contains(T), "{err}");
        assert!(!format!("{err:?}").contains(T), "{err:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Live links

#[test]
fn host_writes_the_endpoint_file_after_binding_and_removes_it() {
    let d = Dir::new("live");
    let path = d.path("host.json");
    let mut host = Link::host(LinkConfig {
        endpoint_file: Some(path.clone()),
        ..cfg("token host")
    })
    .unwrap();
    let e = Endpoint::read(&path).expect("written when the host is created");
    assert_eq!(Some(e.control_port), host.control_addr().map(|a| a.port()));
    assert_eq!(e.token.len(), 64, "a generated token");
    #[cfg(unix)]
    assert_eq!(mode_of(&path), 0o600);
    let mut guest = Link::guest(LinkConfig {
        endpoint_file: Some(path.clone()),
        ..cfg("token guest")
    })
    .unwrap();
    let (eh, eg) = run_until(&mut host, &mut guest, |a, b| connected(a) && connected(b));
    assert!(
        connected(&eh) && connected(&eg),
        "host {eh:?}, guest {eg:?}"
    );
    drop(host);
    assert!(!path.exists(), "removed when the host closes");
    drop(guest);
    assert!(d.names().is_empty());
}

#[test]
fn guest_waits_for_the_file_and_follows_a_restarted_host() {
    let d = Dir::new("restart");
    let path = d.path("host.json");
    let mut guest = Link::guest(LinkConfig {
        endpoint_file: Some(path.clone()),
        ..cfg("token guest")
    })
    .unwrap();
    for _ in 0..20 {
        assert!(guest.poll().is_empty(), "no file yet: nothing happens");
        sleep(Duration::from_millis(5));
    }
    for round in 0..2 {
        let mut host = Link::host(LinkConfig {
            endpoint_file: Some(path.clone()),
            ..cfg("token host")
        })
        .unwrap();
        let (eh, eg) = run_until(&mut host, &mut guest, |a, b| connected(a) && connected(b));
        assert!(
            connected(&eh) && connected(&eg),
            "round {round}: host {eh:?}, guest {eg:?}"
        );
        drop(host); // a new run gets a new port and a new token
    }
}

#[test]
fn a_squatted_port_gets_no_endpoint_file() {
    let d = Dir::new("squat");
    let path = d.path("host.json");
    let squatter = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = squatter.local_addr().unwrap().port();
    let r = Link::host(LinkConfig {
        control_port: port,
        endpoint_file: Some(path.clone()),
        ..cfg("token host")
    });
    assert!(r.is_err());
    assert!(d.names().is_empty(), "the host wrote nothing");
}

#[test]
fn configured_tokens_outside_the_range_are_refused() {
    for t in [
        String::new(),
        "t".repeat(257),
        "é".repeat(129),
        "a\0b".to_owned(),
    ] {
        for host in [true, false] {
            let c = LinkConfig {
                token: Some(t.clone()),
                ..cfg("x")
            };
            let err = if host { Link::host(c) } else { Link::guest(c) }
                .err()
                .expect("refused");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
            if !t.is_empty() {
                assert!(!err.to_string().contains(&t));
            }
        }
    }
    for t in ["t".repeat(256), "é".repeat(128), "x".into()] {
        assert!(Link::host(LinkConfig {
            token: Some(t.clone()),
            ..cfg("x")
        })
        .is_ok());
        assert!(Link::guest(LinkConfig {
            token: Some(t),
            ..cfg("x")
        })
        .is_ok());
    }
}

#[test]
fn a_wrong_token_gets_unauthorized_and_no_token_is_ever_shown() {
    let mut host = Link::host(LinkConfig {
        token: Some(T.into()),
        ..cfg("token host")
    })
    .unwrap();
    let port = host.control_addr().unwrap().port();

    // A raw peer: the answer is exactly the fixed bye.
    let mut raw = TcpStream::connect(("127.0.0.1", port)).unwrap();
    raw.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let hello = format!(
        "{{\"type\":\"hello\",\"role\":\"guest\",\"game\":\"raw\",\"protocol\":1,\"min_protocol\":1,\
         \"udp_port\":9,\"heartbeat_ms\":250,\"timeout_ms\":2000,\"token\":\"{W}\"}}\n"
    );
    raw.write_all(hello.as_bytes()).unwrap();
    let mut answer = String::new();
    let mut events = Vec::new();
    let mut reader = BufReader::new(raw.try_clone().unwrap());
    let end = Instant::now() + Duration::from_secs(3);
    raw.set_nonblocking(false).unwrap();
    while answer.is_empty() && Instant::now() < end {
        events.extend(host.poll());
        reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let _ = reader.read_line(&mut answer);
    }
    assert!(answer.ends_with('\n'), "{answer:?}");
    let bye: serde_json::Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(
        bye,
        serde_json::json!({"type": "bye", "reason": "unauthorized"})
    );
    let end = Instant::now() + Duration::from_millis(300);
    while Instant::now() < end {
        events.extend(host.poll());
        sleep(Duration::from_millis(2));
    }
    assert_eq!(failed(&events), Some(UNAUTHORIZED));

    // A library guest with the wrong token: its event names unauthorized, never a token.
    let mut guest = Link::guest(LinkConfig {
        control_port: port,
        token: Some(W.into()),
        ..cfg("wrong guest")
    })
    .unwrap();
    let (eh, eg) = run_until(&mut host, &mut guest, |a, b| {
        failed(a).is_some() && failed(b).is_some()
    });
    assert!(!connected(&eh) && !connected(&eg));
    assert!(
        failed(&eg).is_some_and(|r| r.contains(UNAUTHORIZED)),
        "{eg:?}"
    );
    for text in [
        format!("{events:?}"),
        format!("{eh:?}"),
        format!("{eg:?}"),
        format!(
            "{:?}",
            LinkConfig {
                token: Some(T.into()),
                ..cfg("x")
            }
        ),
        format!(
            "{:?}",
            Endpoint {
                control_port: 1,
                token: T.into()
            }
        ),
        format!(
            "{:?}",
            Control::parse(&hello).expect("the raw hello parses")
        ),
    ] {
        assert!(!text.contains(T) && !text.contains(W), "{text}");
    }
}
