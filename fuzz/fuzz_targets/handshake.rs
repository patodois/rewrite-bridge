//! The handshake and session state machine of `Link`, fed arbitrary control bytes and UDP datagrams
//! by a raw peer on loopback.
//!
//! Input: byte 0, bit 0 = the Link is a guest (the harness plays the host), bit 1 = the host requires
//! a token. Then a list of operations, each `op: u8, len: u16 (LE), payload`:
//!   0  raw bytes on the control connection
//!   1  raw UDP datagram to the link
//!   2  UDP datagram whose magic, version and session are patched to the live session
//!   3  a well-formed `hello` of the harness peer; payload[0]: 0 no token, 1 the right token,
//!      2 a wrong token of the same length, 3 the right token with its last byte cut
//!      (guest mode: the host's hello, with the session in payload[1..5])
//!   4  poll again after a short pause
//! The invariant checked: a token-protected host never reports `Connected` unless the peer showed its
//! token, either through op 3 mode 1 or because the exact bytes of the token were sent raw.
#![no_main]

use libfuzzer_sys::fuzz_target;
use rewrite_bridge::link::{Event, Link, LinkConfig, MAGIC, MAX_POSES, MAX_TRIANGLES};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::time::Duration;

const TOKEN: &str = "k3y-0123456789abcdef-FUZZ-0123456789";

struct Ops<'a>(&'a [u8]);

impl<'a> Ops<'a> {
    fn next(&mut self) -> Option<(u8, &'a [u8])> {
        let (&op, rest) = self.0.split_first()?;
        let len = u16::from_le_bytes(rest.get(..2)?.try_into().ok()?) as usize;
        let rest = &rest[2..];
        let len = len.min(rest.len());
        let (payload, rest) = rest.split_at(len);
        self.0 = rest;
        Some((op, payload))
    }
}

fn check(events: &[Event], protected: bool, showed_token: bool) {
    for e in events {
        match e {
            Event::Connected(_) => assert!(
                !protected || showed_token,
                "a token-protected host accepted a guest that never showed its token"
            ),
            Event::Poses { poses, .. } => assert!(!poses.is_empty() && poses.len() <= MAX_POSES),
            Event::Collision(c) => assert!(c.triangles.len() <= 4096 * MAX_TRIANGLES),
            _ => {}
        }
    }
}

fn pause() {
    std::thread::sleep(Duration::from_micros(150));
}

fuzz_target!(|data: &[u8]| {
    let Some((&flags, rest)) = data.split_first() else {
        return;
    };
    let guest_mode = flags & 1 != 0;
    let protected = flags & 2 != 0;
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    udp.set_nonblocking(true).unwrap();
    let my_udp = udp.local_addr().unwrap().port();

    let mut cfg = LinkConfig::new("fuzz");
    cfg.control_port = 0;
    cfg.heartbeat = Duration::from_millis(10);
    cfg.timeout = Duration::from_millis(500);
    cfg.reconnect = Duration::from_millis(1);
    if protected {
        cfg.token = Some(TOKEN.to_owned());
    }

    let (mut link, mut stream, _listener);
    if guest_mode {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        cfg.control_port = l.local_addr().unwrap().port();
        link = Link::guest(cfg).unwrap();
        let mut got = None;
        for _ in 0..400 {
            link.poll();
            if let Ok((s, _)) = l.accept() {
                got = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_micros(250));
        }
        let Some(s) = got else { return };
        s.set_nonblocking(true).unwrap();
        stream = s;
        _listener = Some(l);
        // Read the guest's hello line (the link flushes it from a later poll); the token must be in
        // it exactly when configured.
        let mut hello = Vec::new();
        let mut b = [0u8; 256];
        for _ in 0..400 {
            link.poll();
            if let Ok(n) = stream.read(&mut b) {
                hello.extend_from_slice(&b[..n]);
            }
            if hello.ends_with(b"\n") {
                break;
            }
            std::thread::sleep(Duration::from_micros(250));
        }
        let text = String::from_utf8_lossy(&hello);
        assert!(text.ends_with('\n'), "no guest hello: {text}");
        assert_eq!(text.contains(TOKEN), protected, "guest hello: {text}");
    } else {
        link = Link::host(cfg).unwrap();
        stream = TcpStream::connect(link.control_addr().unwrap()).unwrap();
        stream.set_nonblocking(true).unwrap();
        _listener = None;
    }
    let link_udp = link.udp_addr().unwrap();

    let mut sent_raw = Vec::new();
    let mut showed_token = false;
    let mut ops = Ops(rest);
    let mut sink = [0u8; 4096];
    let mut steps = 0;
    while let Some((op, payload)) = ops.next() {
        steps += 1;
        if steps > 64 {
            break;
        }
        match op % 5 {
            0 => {
                // A nonblocking write: if the link stopped reading we simply drop the rest.
                let _ = stream.write(payload);
                sent_raw.extend_from_slice(payload);
                if sent_raw.windows(TOKEN.len()).any(|w| w == TOKEN.as_bytes()) {
                    showed_token = true;
                }
            }
            1 => {
                let _ = udp.send_to(payload, link_udp);
            }
            2 => {
                if let (Some(peer), true) = (link.peer(), payload.len() >= 12) {
                    let mut p = payload.to_vec();
                    p[0..4].copy_from_slice(&MAGIC.to_le_bytes());
                    p[4..6].copy_from_slice(&peer.protocol.to_le_bytes());
                    p[8..12].copy_from_slice(&peer.session.to_le_bytes());
                    let _ = udp.send_to(&p, link_udp);
                }
            }
            3 => {
                let mode = payload.first().copied().unwrap_or(0) % 4;
                let line = if guest_mode {
                    let s = payload
                        .get(1..5)
                        .map_or(1, |b| u32::from_le_bytes(b.try_into().unwrap()));
                    format!(
                        "{{\"type\":\"hello\",\"role\":\"host\",\"game\":\"h\",\"protocol\":1,\"min_protocol\":1,\"udp_port\":{my_udp},\"heartbeat_ms\":10,\"timeout_ms\":500,\"session\":{s}}}\n"
                    )
                } else {
                    let token = match mode {
                        0 => String::new(),
                        1 => format!(",\"token\":\"{TOKEN}\""),
                        2 => format!(",\"token\":\"{}\"", TOKEN.replace('k', "K")),
                        _ => format!(",\"token\":\"{}\"", &TOKEN[..TOKEN.len() - 1]),
                    };
                    if mode == 1 {
                        showed_token = true;
                    }
                    format!(
                        "{{\"type\":\"hello\",\"role\":\"guest\",\"game\":\"g\",\"protocol\":1,\"min_protocol\":1,\"udp_port\":{my_udp},\"heartbeat_ms\":10,\"timeout_ms\":500{token}}}\n"
                    )
                };
                let _ = stream.write(line.as_bytes());
            }
            _ => {}
        }
        pause();
        let events = link.poll();
        check(&events, protected && !guest_mode, showed_token);
        while let Ok(n) = stream.read(&mut sink) {
            if n == 0 {
                break;
            }
        }
        while udp.recv_from(&mut sink).is_ok() {}
    }
    pause();
    let events = link.poll();
    check(&events, protected && !guest_mode, showed_token);
    drop(stream); // so the link's graceful close ends at once instead of after LINGER
    drop(link);
});
