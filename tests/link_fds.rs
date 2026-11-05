//! A host that has run out of file descriptors still takes a guest: its session number needs the operating
//! system's generator, which on Unix means opening `/dev/urandom`, and when that fails the number falls back to
//! a hash instead of the handshake failing (it fails on WASI, where the fallback would be a seed that may be
//! deterministic). This test lowers its own descriptor limit, so it is a test file of its own.
#![cfg(unix)]

use rewrite_bridge::link::{Event, Link, LinkConfig};
use std::io::Write;
use std::net::{TcpStream, UdpSocket};
use std::time::Duration;

#[test]
fn a_host_with_one_descriptor_left_at_the_hello_still_connects() {
    let mut host = Link::host(LinkConfig {
        control_port: 0,
        ..LinkConfig::new("Host")
    })
    .unwrap();
    let port = host.control_addr().unwrap().port();
    let mut tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    let hello = format!(
        "{{\"type\":\"hello\",\"role\":\"guest\",\"game\":\"g\",\"protocol\":1,\"min_protocol\":1,\
         \"udp_port\":{},\"heartbeat_ms\":250,\"timeout_ms\":2000}}\n",
        udp.local_addr().unwrap().port()
    );
    tcp.write_all(hello.as_bytes()).unwrap();
    std::thread::sleep(Duration::from_millis(50));

    // Lower the limit to what is open now plus a few, use every descriptor up, and give one back: the host's
    // accept takes it and the generator has none to open.
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: a valid pointer to an rlimit that lives through the calls.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
        limit.rlim_cur = 64.min(limit.rlim_max);
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0);
    }
    let mut held = Vec::new();
    while let Ok(f) = std::fs::File::open("/dev/null") {
        held.push(f);
    }
    held.pop();
    let mut events = Vec::new();
    for _ in 0..50 {
        events.extend(host.poll());
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(held);
    assert!(
        events.iter().any(|e| matches!(e, Event::Connected(_))),
        "no session: {events:?}"
    );
}
