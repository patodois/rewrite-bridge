//! A fake host: sends an orbiting camera every frame and ground collision every second, reads the
//! guest's poses and frames, and reports what it gets. Run `fake_guest` against it.
//!
//! cargo run --features link --example fake_host -- [--port 47800] [--seconds 10]

use std::thread::sleep;
use std::time::{Duration, Instant};

use rewrite_bridge::link::*;

fn arg<T: std::str::FromStr>(name: &str) -> Option<T> {
    let args: Vec<String> = std::env::args().collect();
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1)?.parse().ok()
}

fn main() -> std::io::Result<()> {
    let port = arg("--port").unwrap_or(DEFAULT_CONTROL_PORT);
    let seconds: f32 = arg("--seconds").unwrap_or(0.0);
    let start = Instant::now();
    let log = |msg: String| println!("[host {:6.2}s] {msg}", start.elapsed().as_secs_f32());

    let mut link = Link::host(LinkConfig {
        control_port: port,
        ..LinkConfig::new("Fake Host")
    })?;
    log(format!("listening on {}", link.control_addr().unwrap()));

    let mut reader: Option<FrameReader> = None;
    let mut pixels = Vec::new();
    let (mut cameras, mut pose_msgs, mut poses, mut frames, mut matched) = (0, 0, 0, 0, 0);
    let mut sent_cameras: Vec<u32> = Vec::new();
    let mut last_pose: Option<Pose> = None;
    let mut latency_us = 0u64;
    let mut next_report = 1.0;
    let mut next_collision = 0.0;
    let mut give_back_at: Option<f32> = None;

    loop {
        let t = start.elapsed().as_secs_f32();
        if seconds > 0.0 && t > seconds {
            break;
        }
        for event in link.poll() {
            match event {
                Event::Connected(p) => {
                    log(format!(
                        "connected: guest \"{}\", protocol v{}, session {:#010x}, guest udp {}",
                        p.game, p.protocol, p.session, p.udp_addr
                    ));
                    next_collision = t;
                }
                Event::Lost { reason } => {
                    log(format!("LOST the guest: {reason}"));
                    reader = None;
                }
                Event::HandshakeFailed { reason } => log(format!("handshake failed: {reason}")),
                Event::Spawn(s) => log(format!(
                    "guest spawned entity {} = {} asset \"{}\"",
                    s.entity, s.provider, s.asset
                )),
                Event::Despawn { entity } => log(format!("guest despawned entity {entity}")),
                Event::Poses { header, poses: ps } => {
                    pose_msgs += 1;
                    poses += ps.len();
                    latency_us = now_us().saturating_sub(header.time_us);
                    last_pose = ps.last().copied();
                }
                Event::Focus {
                    focus,
                    requested: true,
                } => {
                    log(format!(
                        "guest asks for input focus; granting it to the {}",
                        focus.as_str()
                    ));
                    let _ = link.set_focus(focus);
                    give_back_at = Some(t + 2.0);
                }
                Event::FrameChannel(c) => {
                    log(format!(
                        "guest pixel file {} ({}x{}, {} slots)",
                        c.path, c.width, c.height, c.slots
                    ));
                    reader = FrameReader::open(c.path.as_ref()).ok();
                }
                Event::Frame { frame, .. } => {
                    if let Some(r) = &reader {
                        if let Ok(d) = r.read(&frame, &mut pixels) {
                            frames += 1;
                            if sent_cameras.contains(&d.camera_seq) {
                                matched += 1;
                            }
                            if frames == 1 {
                                log(format!(
                                    "sample frame #{} {}x{}, first pixel {:?}, rendered for camera seq {}",
                                    d.frame, d.width, d.height, &pixels[..4], d.camera_seq
                                ));
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        if link.is_connected() {
            let a = 0.5 * t;
            let eye = [10.0 * a.cos(), 3.0, 10.0 * a.sin()];
            let cam = Camera::look_at(eye, [0.0, 1.0, 0.0], 1.0, 0.1, 1000.0, [1280, 720]);
            match link.send_camera(&cam) {
                Ok(seq) => {
                    cameras += 1;
                    sent_cameras.push(seq);
                    if sent_cameras.len() > 120 {
                        sent_cameras.remove(0);
                    }
                }
                Err(e) => log(format!("send_camera: {e}")),
            }
            if t >= next_collision {
                next_collision = t + 1.0;
                // A flat 20 x 20 m ground of 1 m quads under the camera: 800 triangles, 4 chunks.
                let (cx, cz) = (eye[0].round(), eye[2].round());
                let mut tris = Vec::new();
                for i in -10..10 {
                    for j in -10..10 {
                        let (x, z) = (cx + i as f32, cz + j as f32);
                        tris.push([[x, 0.0, z], [x, 0.0, z + 1.0], [x + 1.0, 0.0, z + 1.0]]);
                        tris.push([[x, 0.0, z], [x + 1.0, 0.0, z + 1.0], [x + 1.0, 0.0, z]]);
                    }
                }
                let _ = link.send_collision([cx, 0.0, cz], 10.0, &tris);
            }
            if give_back_at.is_some_and(|g| t >= g) {
                give_back_at = None;
                log("taking input focus back".into());
                let _ = link.set_focus(Role::Host);
            }
        }

        if t >= next_report {
            next_report += 1.0;
            if link.is_connected() {
                let s = link.stats();
                log(format!(
                    "sent {cameras} cameras; got {poses} poses in {pose_msgs} datagrams, {frames} frames \
                     ({matched} matched to a sent camera); last pose {:?}; pose latency {latency_us} us; \
                     malformed {} stale {} foreign {}",
                    last_pose.map(|p| p.translation.map(|x| (x * 100.0).round() / 100.0)),
                    s.malformed,
                    s.stale,
                    s.foreign
                ));
            }
            (cameras, pose_msgs, poses, frames, matched) = (0, 0, 0, 0, 0);
        }
        sleep(Duration::from_millis(16));
    }
    log("done".into());
    Ok(())
}
