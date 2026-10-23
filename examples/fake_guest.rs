//! A fake guest: a lit cube standing at a fixed place in the host's world. For every camera the
//! host sends it answers with the cube's pose and, with `--frames`, renders the cube with that
//! camera on the CPU (colour and linear depth, 320x240) into a pixel file and announces it with
//! the camera's `seq`, as the contract describes. `--latency` holds each frame back before it is
//! sent, to imitate a slow renderer, so the host has something to reproject.
//!
//! By default the cube stands at the world origin (what `fake_host` expects). With `--in-front`
//! it is placed 4 m ahead of the first camera the host sends, and stands on the host's collision
//! surface below that point once a `Collision` snapshot covers it (until then it floats at eye
//! height). Use that with a real game, whose player is rarely near the origin.
//!
//! cargo run --release --features link --example fake_guest -- [--port 47800] [--seconds 10]
//!     [--frames [--no-depth] [--latency 40]] [--in-front]     (latency in milliseconds)

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

/// Where the cube stands by default, in the host's world: on the ground at the origin, 1.6 m tall.
const CUBE_CENTER: [f32; 3] = [0.0, 0.8, 0.0];
const CUBE_HALF: f32 = 0.8;
/// How far ahead of the first camera `--in-front` puts the cube, in metres.
const IN_FRONT: f32 = 4.0;
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
fn render(camera: &Camera, center: [f32; 3], yaw: f32) -> Target {
    let mut target = Target::new(SIZE.0 as usize, SIZE.1 as usize);
    target.draw(camera, &cuboid(center, [CUBE_HALF; 3], yaw), &|_, n| {
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
    });
    target
}

/// The point `IN_FRONT` metres ahead of the camera along its horizontal heading, at eye height.
fn ahead_of(camera: &Camera) -> [f32; 3] {
    let (eye, f) = (camera.eye(), camera.forward());
    let len = (f[0] * f[0] + f[2] * f[2]).sqrt();
    let (dx, dz) = if len > 1e-3 {
        (f[0] / len, f[2] / len)
    } else {
        (0.0, -1.0)
    };
    [eye[0] + dx * IN_FRONT, eye[1], eye[2] + dz * IN_FRONT]
}

/// The height of the highest collision triangle under `(x, z)` that is not above `max_y`.
fn ground_under(triangles: &[Triangle], x: f32, z: f32, max_y: f32) -> Option<f32> {
    triangles
        .iter()
        .filter_map(|[a, b, c]| {
            // Barycentric coordinates of (x, z) in the triangle projected onto the ground plane
            let det = (b[2] - c[2]) * (a[0] - c[0]) + (c[0] - b[0]) * (a[2] - c[2]);
            if det.abs() < 1e-9 {
                return None;
            }
            let u = ((b[2] - c[2]) * (x - c[0]) + (c[0] - b[0]) * (z - c[2])) / det;
            let v = ((c[2] - a[2]) * (x - c[0]) + (a[0] - c[0]) * (z - c[2])) / det;
            let w = 1.0 - u - v;
            (u >= 0.0 && v >= 0.0 && w >= 0.0).then(|| u * a[1] + v * b[1] + w * c[1])
        })
        .filter(|&y| y <= max_y)
        .max_by(f32::total_cmp)
}

fn main() -> std::io::Result<()> {
    let port = arg("--port").unwrap_or(DEFAULT_CONTROL_PORT);
    let seconds: f32 = arg("--seconds").unwrap_or(0.0);
    let frames = std::env::args().any(|a| a == "--frames");
    let with_depth = !std::env::args().any(|a| a == "--no-depth");
    let latency = Duration::from_millis(arg("--latency").unwrap_or(40));
    let in_front = std::env::args().any(|a| a == "--in-front");
    // With --in-front: chosen from the first camera, then set on the ground once collision is known
    let mut center = (!in_front).then_some(CUBE_CENTER);
    let mut grounded = !in_front;
    let mut last_collision: Vec<Triangle> = Vec::new();
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
                    if in_front {
                        (center, grounded) = (None, false);
                    }
                    writer = None;
                    pending.clear();
                }
                Event::HandshakeFailed { reason } => log(format!("handshake failed: {reason}")),
                Event::Camera { header, camera } => {
                    cameras += 1;
                    newest = Some((header.seq, camera));
                }
                Event::Collision(c) => {
                    log(format!(
                        "collision query {}: {} triangles within {} m of {:?}",
                        c.query,
                        c.triangles.len(),
                        c.radius,
                        c.center
                    ));
                    last_collision = c.triangles;
                }
                Event::Focus { focus, .. } => log(format!(
                    "host says input focus is now with the {}",
                    focus.as_str()
                )),
                _ => {}
            }
        }

        if let Some((_, camera)) = &newest {
            if center.is_none() {
                let at = ahead_of(camera);
                log(format!(
                    "cube placed {IN_FRONT} m ahead of the camera at {:?}",
                    at.map(|v| (v * 100.0).round() / 100.0)
                ));
                center = Some(at);
            }
        }
        if let (Some(at), false) = (center, grounded) {
            if let Some(y) = ground_under(&last_collision, at[0], at[2], at[1] + 0.5) {
                center = Some([at[0], y + CUBE_HALF, at[2]]);
                grounded = true;
                log(format!("cube stands on the host's ground at y {y:.2}"));
            }
        }
        if let (Some((seq, camera)), Some(center)) = (newest, center) {
            let now = now_us();
            let yaw = 0.6 * t;
            let mut pose = Pose::new(CUBE, now);
            pose.translation = center;
            pose.rotation = [0.0, (yaw * 0.5).sin(), 0.0, (yaw * 0.5).cos()];
            if link.send_poses(&[pose]).is_ok() {
                poses_sent += 1;
            }
            if writer.is_some() {
                let began = Instant::now();
                let target = render(&camera, center, yaw);
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
