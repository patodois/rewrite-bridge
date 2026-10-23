//! A fake host: sweeps a camera sideways in front of a small synthetic scene (a checkered floor and a
//! pillar, rasterised on the CPU), sends it every frame and ground collision every second, reads
//! the guest's poses, and presents the guest's frames the way the contract describes (section
//! 9.7): look up the camera a frame was rendered with by its `camera_seq`, reproject the frame to
//! the newest camera, convert the host's depth buffer to linear metres, and let the nearer pixel
//! win. Run `fake_guest --frames` against it.
//!
//! cargo run --release --features link --example fake_host -- [--port 47800] [--seconds 10]
//!     [--dump DIR [--dump-every 25] [--dump-max 6]] [--stats-every 10] [--big-collision CHUNKS]
//!
//! `--big-collision N` sends a snapshot of N full chunks (N * 226 triangles) each second instead of
//! the small ground patch, to exercise the sender's pacing and the guest's reassembly.

#[path = "common/raster.rs"]
mod raster;

use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use raster::{cuboid, floor, lit, write_ppm, Target};
use rewrite_bridge::link::compose::{
    composite, delinearize_depth, linearize_buffer, reproject, CameraRing,
};
use rewrite_bridge::link::*;

fn arg<T: std::str::FromStr>(name: &str) -> Option<T> {
    let args: Vec<String> = std::env::args().collect();
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1)?.parse().ok()
}

const SIZE: (usize, usize) = (320, 240);

/// The host's camera at time `t`: sweeps sideways while looking at the middle of the scene, with
/// the reversed-Z, infinite-far projection many engines use.
fn camera_at(t: f32) -> Camera {
    let eye = [1.4 * (0.9 * t).sin(), 1.8, 5.5];
    Camera::look_at(
        eye,
        [0.0, 0.9, 0.0],
        1.0,
        0.1,
        f32::INFINITY,
        [SIZE.0 as u32, SIZE.1 as u32],
    )
    .with_depth_range(DepthRange::Reversed)
}

/// The host's own scene: sky, checkered floor, and a stone pillar standing between the camera and
/// where the guest's cube is.
fn render_scene(camera: &Camera) -> Target {
    let mut target = Target::new(SIZE.0, SIZE.1);
    target.sky([110, 160, 220], [215, 225, 235]);
    target.draw(
        camera,
        &floor(0.0, [-30.0, 30.0], [-40.0, 12.0]),
        &|p, n| {
            let dark = (p[0].floor() as i32 + p[2].floor() as i32) & 1 == 0;
            lit(
                if dark {
                    [96.0, 104.0, 98.0]
                } else {
                    [176.0, 182.0, 170.0]
                },
                n,
            )
        },
    );
    target.draw(
        camera,
        &cuboid([-0.25, 1.5, 2.6], [0.2, 1.5, 0.2], 0.0),
        &|_, n| lit([205.0, 190.0, 160.0], n),
    );
    target
}

/// A guest frame as last read from the pixel file.
struct GuestFrame {
    rgba: Vec<u8>,
    depth: Vec<f32>,
    number: u64,
    camera_seq: u32,
    time_us: u64,
    width: u32,
    height: u32,
}

fn main() -> std::io::Result<()> {
    let port = arg("--port").unwrap_or(DEFAULT_CONTROL_PORT);
    let seconds: f32 = arg("--seconds").unwrap_or(0.0);
    let dump: Option<PathBuf> = arg("--dump");
    let dump_every: u32 = arg("--dump-every").unwrap_or(25);
    let dump_max: u32 = arg("--dump-max").unwrap_or(6);
    let stats_every: u32 = arg("--stats-every").unwrap_or(10);
    let big_collision: Option<usize> = arg("--big-collision");
    if let Some(dir) = &dump {
        std::fs::create_dir_all(dir)?;
    }
    let start = Instant::now();
    let log = |msg: String| println!("[host {:6.2}s] {msg}", start.elapsed().as_secs_f32());

    let mut link = Link::host(LinkConfig {
        control_port: port,
        ..LinkConfig::new("Fake Host")
    })?;
    log(format!("listening on {}", link.control_addr().unwrap()));

    let mut reader: Option<FrameReader> = None;
    let (mut pixels, mut depth) = (Vec::new(), Vec::new());
    let mut guest: Option<GuestFrame> = None;
    let mut ring = CameraRing::default();
    let (mut cameras, mut pose_msgs, mut poses, mut frames) = (0, 0, 0, 0);
    let mut last_pose: Option<Pose> = None;
    let mut latency_us = 0u64;
    let (mut presented, mut dumped) = (0u32, 0u32);
    let (mut age_sum, mut age_max, mut age_n) = (0.0f32, 0.0f32, 0u32);
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
                    guest = None;
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
                    reader = FrameReader::open(c.path.as_ref()).ok();
                    log(format!(
                        "guest pixel file {} ({}x{}, {} slots, depth plane: {})",
                        c.path,
                        c.width,
                        c.height,
                        c.slots,
                        reader.as_ref().is_some_and(|r| r.has_depth())
                    ));
                }
                Event::Frame { frame, .. } => {
                    if let Some(r) = &reader {
                        if let Ok(d) = r.read_depth(&frame, &mut pixels, &mut depth) {
                            frames += 1;
                            guest = Some(GuestFrame {
                                rgba: pixels.clone(),
                                depth: depth.clone(),
                                number: d.frame,
                                camera_seq: d.camera_seq,
                                time_us: d.time_us,
                                width: d.width,
                                height: d.height,
                            });
                        }
                    }
                }
                _ => {}
            }
        }

        if link.is_connected() {
            let camera = camera_at(t);
            match link.send_camera(&camera) {
                Ok(seq) => {
                    cameras += 1;
                    ring.push(seq, camera);
                }
                Err(e) => log(format!("send_camera: {e}")),
            }
            if t >= next_collision {
                next_collision = t + 1.0;
                // A flat 20 x 20 m ground of 1 m quads around the camera: 800 triangles, 4 chunks.
                let eye = camera.eye();
                let (cx, cz) = (eye[0].round(), eye[2].round());
                let mut tris: Vec<Triangle> = Vec::new();
                for i in -10..10 {
                    for j in -10..10 {
                        let (x, z) = (cx + i as f32, cz + j as f32);
                        tris.push([[x, 0.0, z], [x, 0.0, z + 1.0], [x + 1.0, 0.0, z + 1.0]]);
                        tris.push([[x, 0.0, z], [x + 1.0, 0.0, z + 1.0], [x + 1.0, 0.0, z]]);
                    }
                }
                if let Some(chunks) = big_collision {
                    tris = (0..chunks * MAX_TRIANGLES)
                        .map(|i| {
                            let x = (i % 4096) as f32;
                            [[x, 0.0, 0.0], [x + 1.0, 0.0, 0.0], [x, 0.0, 1.0]]
                        })
                        .collect();
                }
                let _ = link.send_collision([cx, 0.0, cz], 10.0, &tris);
            }
            if give_back_at.is_some_and(|g| t >= g) {
                give_back_at = None;
                log("taking input focus back".into());
                let _ = link.set_focus(Role::Host);
            }

            // Present: the host's scene, then the guest's latest frame warped to this camera.
            if let Some(g) = guest.as_ref().filter(|g| {
                (g.width as usize, g.height as usize) == SIZE && g.rgba.len() == SIZE.0 * SIZE.1 * 4
            }) {
                let scene = render_scene(&camera);
                // What an engine has: a depth buffer in the camera's own convention. Convert it
                // to linear metres, the unit the guest's depth is in.
                let buffer: Vec<f32> = scene
                    .depth
                    .iter()
                    .map(|&d| delinearize_depth(d, camera.near, camera.far, camera.depth_range))
                    .collect();
                let host_depth = linearize_buffer(&buffer, &camera);

                let render_cam = ring.get(g.camera_seq);
                let n = SIZE.0 * SIZE.1;
                let (rgba, dep) = if g.depth.len() == n {
                    // An unknown camera_seq composites as if the guest used the current camera.
                    let (rgba, dep) = reproject(
                        &g.rgba,
                        &g.depth,
                        g.width,
                        g.height,
                        render_cam.unwrap_or(&camera),
                        &camera,
                        SIZE.0 as u32,
                        SIZE.1 as u32,
                    )
                    .expect("sizes checked above");
                    (rgba, dep)
                } else {
                    // A colour-only guest: nothing to test against, so paste it over.
                    (g.rgba.clone(), vec![1e-6; n])
                };
                let out = composite(&scene.rgba, &host_depth, &rgba, &dep).expect("same size");
                presented += 1;
                let age_ms = now_us().saturating_sub(g.time_us) as f32 / 1000.0;
                age_sum += age_ms;
                age_max = age_max.max(age_ms);
                age_n += 1;
                if stats_every > 0 && presented % stats_every == 0 {
                    log(format!(
                        "present #{presented}: guest frame {} (camera seq {}, {}), age {age_ms:.1} ms, \
                         guest pixels covering the host {}, hidden by the host {}",
                        g.number,
                        g.camera_seq,
                        match (g.depth.len() == n, render_cam.is_some()) {
                            (false, _) => "colour only, pasted over",
                            (true, true) => "reprojected",
                            (true, false) => "camera not in the ring, composited as is",
                        },
                        out.guest_in_front,
                        out.guest_hidden,
                    ));
                }
                if let Some(dir) = dump
                    .as_ref()
                    .filter(|_| dumped < dump_max && presented % dump_every == 0)
                {
                    dumped += 1;
                    // For comparison, the same guest frame (a) pasted over the host's image with
                    // no depth and no reprojection, and (b) depth-tested but not reprojected, as
                    // it was rendered for the older camera.
                    let paste = composite(&scene.rgba, &host_depth, &g.rgba, &vec![1e-6; n])
                        .expect("same size");
                    let stale = (g.depth.len() == n)
                        .then(|| composite(&scene.rgba, &host_depth, &g.rgba, &g.depth))
                        .and_then(Result::ok);
                    let save = |name: &str, rgba: &[u8]| {
                        write_ppm(
                            &dir.join(format!("{name}_{presented:04}.ppm")),
                            SIZE.0,
                            SIZE.1,
                            rgba,
                        )
                    };
                    save("composite", &out.rgba)?;
                    save("pasted", &paste.rgba)?;
                    if let Some(stale) = stale {
                        save("stale", &stale.rgba)?;
                    }
                    log(format!(
                        "wrote {}/composite_{presented:04}.ppm",
                        dir.display()
                    ));
                }
            }
        }

        if t >= next_report {
            next_report += 1.0;
            if link.is_connected() {
                let s = link.stats();
                log(format!(
                    "sent {cameras} cameras; got {poses} poses in {pose_msgs} datagrams, {frames} frames; \
                     presented {age_n} (age mean {:.1} ms, max {age_max:.1} ms); last pose {:?}; \
                     pose latency {latency_us} us; malformed {} stale {} foreign {}",
                    age_sum / age_n.max(1) as f32,
                    last_pose.map(|p| p.translation.map(|x| (x * 100.0).round() / 100.0)),
                    s.malformed,
                    s.stale,
                    s.foreign
                ));
            }
            (cameras, pose_msgs, poses, frames) = (0, 0, 0, 0);
            (age_sum, age_max, age_n) = (0.0, 0.0, 0);
        }
        sleep(Duration::from_millis(16));
    }
    log("done".into());
    Ok(())
}
