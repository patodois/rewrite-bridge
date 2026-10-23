//! A fake guest: a lit cube standing at a fixed place in the host's world. For every camera the
//! host sends it answers with the cube's pose and, with `--frames`, renders the cube with that
//! camera on the CPU (colour and linear depth, 320x240) into a pixel file and announces it with
//! the camera's `seq`, as the contract describes. `--latency` holds each frame back before it is
//! sent, to imitate a slow renderer, so the host has something to reproject.
//!
//! cargo run --release --features link --example fake_guest -- [--port 47800] [--seconds 10]
//!     [--frames [--no-depth] [--latency 40]]     (latency in milliseconds)

#[path = "common/raster.rs"]
mod raster;

use std::collections::VecDeque;
use std::thread::sleep;
use std::time::{Duration, Instant};

use raster::{cuboid, lit, Target};
use rewrite_bridge::link::*;

fn arg<T: std::str::FromStr>(name: &str) -> Option<T> {
    let args: Vec<String> = std::env::args().collect();
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1)?.parse().ok()
}

/// Where the cube stands, in the host's world: on the ground, 1.6 m tall.
const CUBE_CENTER: [f32; 3] = [0.0, 0.8, 0.0];
const CUBE_HALF: f32 = 0.8;
const SIZE: (u32, u32) = (320, 240);

/// A frame rendered and waiting for its (imitated) render latency to pass.
struct Pending {
    due: Instant,
    rgba: Vec<u8>,
    depth: Vec<f32>,
    camera_seq: u32,
    time_us: u64,
}

/// Renders the cube as seen by `camera`: opaque where it is, alpha 0 and depth +inf elsewhere.
fn render(camera: &Camera, yaw: f32) -> Target {
    let mut target = Target::new(SIZE.0 as usize, SIZE.1 as usize);
    target.draw(
        camera,
        &cuboid(CUBE_CENTER, [CUBE_HALF; 3], yaw),
        &|_, n| {
            // One colour per face direction, lit by a fixed light.
            let base = [
                [235.0, 80.0, 70.0],
                [70.0, 200.0, 110.0],
                [80.0, 130.0, 240.0],
            ];
            let (axis, sign) = (0..3)
                .map(|a| (a, n[a]))
                .max_by(|x, y| x.1.abs().total_cmp(&y.1.abs()))
                .unwrap();
            let mut c = base[axis];
            if sign < 0.0 {
                c = c.map(|v| v * 0.7 + 60.0);
            }
            lit(c, n)
        },
    );
    target
}

fn main() -> std::io::Result<()> {
    let port = arg("--port").unwrap_or(DEFAULT_CONTROL_PORT);
    let seconds: f32 = arg("--seconds").unwrap_or(0.0);
    let frames = std::env::args().any(|a| a == "--frames");
    let with_depth = !std::env::args().any(|a| a == "--no-depth");
    let latency = Duration::from_millis(arg("--latency").unwrap_or(40));
    let start = Instant::now();
    let log = |msg: String| println!("[guest {:6.2}s] {msg}", start.elapsed().as_secs_f32());

    let mut link = Link::guest(LinkConfig {
        control_port: port,
        ..LinkConfig::new("Fake Guest")
    })?;
    log(format!("connecting to 127.0.0.1:{port}"));

    const CUBE: u32 = 1;
    let mut writer: Option<FrameWriter> = None;
    let mut pending: VecDeque<Pending> = VecDeque::new();
    let (mut cameras, mut poses_sent, mut frames_sent) = (0, 0, 0);
    let mut render_ms = 0.0;
    let mut ask_focus_at: Option<f32> = None;
    let mut next_report = 1.0;

    loop {
        let t = start.elapsed().as_secs_f32();
        if seconds > 0.0 && t > seconds {
            break;
        }
        // Only the newest camera of this batch is worth rendering.
        let mut newest: Option<(u32, Camera)> = None;
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
                        let mut opts = FrameFileOptions::new(SIZE.0, SIZE.1, 3);
                        if with_depth {
                            opts = opts.with_depth();
                        }
                        match FrameWriter::create_temp_with(&tag, &opts) {
                            Ok(wr) => {
                                let _ = link.announce_frames(wr.channel());
                                log(format!(
                                    "pixel file {} ({}x{}, depth: {with_depth}, latency {} ms)",
                                    wr.channel().path,
                                    SIZE.0,
                                    SIZE.1,
                                    latency.as_millis()
                                ));
                                writer = Some(wr);
                            }
                            Err(e) => log(format!("no pixel file: {e}")),
                        }
                    }
                    ask_focus_at = Some(t + 2.0);
                }
                Event::Lost { reason } => {
                    log(format!("LOST the host: {reason}"));
                    writer = None;
                    pending.clear();
                }
                Event::HandshakeFailed { reason } => log(format!("handshake failed: {reason}")),
                Event::Camera { header, camera } => {
                    cameras += 1;
                    newest = Some((header.seq, camera));
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

        if let Some((seq, camera)) = newest {
            let now = now_us();
            let yaw = 0.6 * t;
            let mut pose = Pose::new(CUBE, now);
            pose.translation = CUBE_CENTER;
            pose.rotation = [0.0, (yaw * 0.5).sin(), 0.0, (yaw * 0.5).cos()];
            if link.send_poses(&[pose]).is_ok() {
                poses_sent += 1;
            }
            if writer.is_some() {
                let began = Instant::now();
                let target = render(&camera, yaw);
                render_ms += began.elapsed().as_secs_f32() * 1000.0;
                pending.push_back(Pending {
                    due: Instant::now() + latency,
                    rgba: target.rgba,
                    depth: target.depth,
                    camera_seq: seq,
                    time_us: now,
                });
            }
        }
        while pending.front().is_some_and(|p| p.due <= Instant::now()) {
            let p = pending.pop_front().unwrap();
            if let Some(wr) = &mut writer {
                let depth = wr.has_depth().then_some(p.depth.as_slice());
                let sent = wr.write_depth(&p.rgba, depth, SIZE.0, SIZE.1, p.camera_seq, p.time_us);
                if let Ok(ready) = sent {
                    if link.send_frame(&ready).is_ok() {
                        frames_sent += 1;
                    }
                }
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
                    "got {cameras} cameras; sent {poses_sent} poses, {frames_sent} frames \
                     (render {:.1} ms each); focus with the {}",
                    render_ms / frames_sent.max(1) as f32,
                    link.focus().as_str()
                ));
            }
            (cameras, poses_sent, frames_sent) = (0, 0, 0);
            render_ms = 0.0;
        }
        sleep(Duration::from_millis(2));
    }
    log("done".into());
    Ok(())
}
