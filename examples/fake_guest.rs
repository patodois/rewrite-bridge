//! A fake guest: answers every camera from the host with the pose of a cube floating 3 m in front
//! of it, optionally renders a tiny frame per camera into a pixel file, and asks for input focus.
//!
//! cargo run --features link --example fake_guest -- [--port 47800] [--seconds 10] [--frames]

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
    let frames = std::env::args().any(|a| a == "--frames");
    let start = Instant::now();
    let log = |msg: String| println!("[guest {:6.2}s] {msg}", start.elapsed().as_secs_f32());

    let mut link = Link::guest(LinkConfig {
        control_port: port,
        ..LinkConfig::new("Fake Guest")
    })?;
    log(format!("connecting to 127.0.0.1:{port}"));

    const CUBE: u32 = 1;
    let (w, h) = (64u32, 36u32);
    let mut writer: Option<FrameWriter> = None;
    let mut image = vec![0u8; (w * h * 4) as usize];
    let (mut cameras, mut poses_sent, mut frames_sent) = (0, 0, 0);
    let mut last: Option<([f32; 3], u64)> = None;
    let mut ask_focus_at: Option<f32> = None;
    let mut next_report = 1.0;

    loop {
        let t = start.elapsed().as_secs_f32();
        if seconds > 0.0 && t > seconds {
            break;
        }
        for event in link.poll() {
            match event {
                Event::Connected(p) => {
                    log(format!(
                        "connected: host \"{}\", protocol v{}, session {:#010x}",
                        p.game, p.protocol, p.session
                    ));
                    let _ = link.spawn(&Spawn {
                        entity: CUBE,
                        provider: "Fake Guest".into(),
                        asset: "demo/cube".into(),
                        name: Some("Cube".into()),
                    });
                    if frames {
                        let tag = format!("guest-{:08x}", p.session);
                        match FrameWriter::create_temp(&tag, w, h, 3) {
                            Ok(wr) => {
                                let _ = link.announce_frames(wr.channel());
                                writer = Some(wr);
                            }
                            Err(e) => log(format!("no pixel file: {e}")),
                        }
                    }
                    ask_focus_at = Some(t + 2.0);
                    last = None;
                }
                Event::Lost { reason } => {
                    log(format!("LOST the host: {reason}"));
                    writer = None;
                }
                Event::HandshakeFailed { reason } => log(format!("handshake failed: {reason}")),
                Event::Camera { header, camera } => {
                    cameras += 1;
                    let (eye, fwd) = (camera.eye(), camera.forward());
                    let pos: [f32; 3] = std::array::from_fn(|i| eye[i] + 3.0 * fwd[i]);
                    let now = now_us();
                    let mut pose = Pose::new(CUBE, now);
                    pose.translation = pos;
                    if let Some((p0, t0)) = last {
                        let dt = (now - t0) as f32 * 1e-6;
                        if dt > 0.0 {
                            pose.velocity = std::array::from_fn(|i| (pos[i] - p0[i]) / dt);
                        }
                    }
                    last = Some((pos, now));
                    if link.send_poses(&[pose]).is_ok() {
                        poses_sent += 1;
                    }
                    if let Some(wr) = &mut writer {
                        let shade = (header.seq % 256) as u8;
                        for px in image.as_chunks_mut::<4>().0 {
                            px.copy_from_slice(&[shade, 128, 255 - shade, 255]);
                        }
                        if let Ok(ready) = wr.write(&image, w, h, header.seq, now) {
                            if link.send_frame(&ready).is_ok() {
                                frames_sent += 1;
                            }
                        }
                    }
                }
                Event::Collision(c) => log(format!(
                    "collision query {}: {} triangles within {} m of {:?}",
                    c.query,
                    c.triangles.len(),
                    c.radius,
                    c.center
                )),
                Event::Focus { focus, .. } => log(format!(
                    "host says input focus is now with the {}",
                    focus.as_str()
                )),
                _ => {}
            }
        }
        if ask_focus_at.is_some_and(|a| t >= a) {
            ask_focus_at = None;
            log("asking the host for input focus".into());
            let _ = link.set_focus(Role::Guest);
        }
        if t >= next_report {
            next_report += 1.0;
            if link.is_connected() {
                log(format!(
                    "got {cameras} cameras; sent {poses_sent} poses, {frames_sent} frames; focus with the {}",
                    link.focus().as_str()
                ));
            }
            (cameras, poses_sent, frames_sent) = (0, 0, 0);
        }
        sleep(Duration::from_millis(4));
    }
    log("done".into());
    Ok(())
}
