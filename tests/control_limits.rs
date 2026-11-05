#![cfg(feature = "link")]
use rewrite_bridge::link::*;
use std::time::{Duration, Instant};

#[test]
fn oversized_control_is_refused_locally_and_the_session_survives() {
    let config = |game: &str, control_port| LinkConfig {
        game: game.into(),
        control_port,
        timeout: Duration::from_secs(30),
        heartbeat: Duration::from_secs(10),
        ..LinkConfig::new(game)
    };
    let mut host = Link::host(config("host", 0)).unwrap();
    let mut guest = Link::guest(config("guest", host.control_addr().unwrap().port())).unwrap();
    let end = Instant::now() + Duration::from_secs(3);
    while !(host.is_connected() && guest.is_connected()) && Instant::now() < end {
        host.poll();
        guest.poll();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(host.is_connected() && guest.is_connected());
    for asset in ["a".repeat(MAX_LINE), "é".repeat(MAX_LINE / 2)] {
        let before = guest.stats().lines_sent;
        assert!(matches!(
            guest.spawn(&Spawn {
                entity: 1,
                provider: "p".into(),
                asset,
                name: None
            }),
            Err(LinkError::Wire(_))
        ));
        assert_eq!(guest.stats().lines_sent, before);
        assert!(guest.is_connected());
    }
    guest
        .spawn(&Spawn {
            entity: 2,
            provider: "p".into(),
            asset: "valid".into(),
            name: None,
        })
        .unwrap();
    let end = Instant::now() + Duration::from_secs(1);
    loop {
        if host.poll().iter().any(|e| matches!(e, Event::Spawn(_))) {
            break;
        }
        assert!(
            Instant::now() < end,
            "valid control after local rejection was lost"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
