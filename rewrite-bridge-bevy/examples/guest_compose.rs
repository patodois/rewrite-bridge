//! A host Bevy scene that lets a guest game draw into it, with depth: host geometry hides the guest where
//! it is nearer and the guest hides host geometry where it is nearer.
//!
//! ```text
//! cargo run --release --example guest_compose --features compose,window      # in a window
//! cargo run --release --example guest_compose --features compose,viewer -- --screenshot out.png
//! ```
//!
//! The host is a small Bevy scene (a ground, a wall, a pillar, a crate) with `GuestComposePlugin` and a
//! `HostLinkPlugin` listening for a guest. The guest is, by default, a thread of this same program that
//! speaks the passthrough link like any other guest would (docs/CONTRACT.md): it receives the host's camera,
//! ray-casts two spheres on the CPU with exactly that camera, writes colour and linear depth into a pixel
//! file and announces it. One sphere is sunk into the wall (the wall hides what is behind it, the sphere
//! covers the wall around it, the pillar in front hides its right side), a smaller one circles in front of
//! the wall, sinks into the ground and passes behind the pillar.
//!
//! The camera swings from side to side. `--latency 120` holds each guest frame back by 120 ms, as a slow
//! renderer would, so a frame arrives rendered for an older camera. The plugin then reprojects it to the
//! camera now (the default); with `--no-reproject` it does not, and the guest's layer visibly lags the host's.
//! `--no-depth` makes the guest send colour only, which the contract pastes over the host's image.
//!
//! Options:
//!
//! - `--size 960x540`: the window, or the off-screen picture
//! - `--latency MS`: how long the built-in guest holds each frame back (default 100)
//! - `--no-reproject`: composite late frames as they are, without warping them to the current camera
//! - `--no-depth`: the guest sends colour only; the contract pastes such a frame over the host's image
//! - `--swing SPEED`: how fast the camera swings, radians per second (default 0.9)
//! - `--screenshot FILE`: off screen, no window: waits for the guest, takes the picture about four seconds
//!   into the swing and exits
//! - `--no-guest`: no built-in guest; with `--screenshot` that is the host's own picture
//! - `--seconds N`: quit after N seconds (in a window; the default there is to run until closed)
//! - `--external [--port 47800]`: no built-in guest, wait on that port for another one (`bridge-guest`, or the
//!   `fake_guest` example of rewrite-bridge with `--frames`) to connect
//!
//! In a window, R toggles reprojection.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bevy::app::ScheduleRunnerPlugin;
use bevy::asset::RenderAssetUsages;
use bevy::camera::RenderTarget;
use bevy::core_pipeline::tonemapping::{DebandDither, Tonemapping};
use bevy::prelude::*;
use bevy::render::render_resource::TextureFormat;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};
use bevy::window::ExitCondition;
use rewrite_bridge::link::{
    now_us, Camera as HostCamera, Event, FrameFileOptions, FrameWriter, Link, LinkConfig,
};
use rewrite_bridge_bevy::compose::{
    GuestCompose, GuestComposePlugin, GuestFrames, HostLink, HostLinkPlugin,
};

// ---------------------------------------------------------------------------------------------
// Options

#[derive(Resource, Clone)]
struct Args {
    screenshot: Option<PathBuf>,
    size: (u32, u32),
    latency: Duration,
    reproject: bool,
    guest: bool,
    depth: bool,
    swing: f32,
    external: bool,
    port: u16,
    seconds: Option<f32>,
}

fn parse_args() -> Args {
    let mut a = Args {
        screenshot: None,
        size: (960, 540),
        latency: Duration::from_millis(100),
        reproject: true,
        guest: true,
        depth: true,
        swing: 0.9,
        external: false,
        port: rewrite_bridge::link::DEFAULT_CONTROL_PORT,
        seconds: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |what: &str| it.next().unwrap_or_else(|| panic!("{arg} needs {what}"));
        match arg.as_str() {
            "--screenshot" => a.screenshot = Some(PathBuf::from(value("a path"))),
            "--size" => {
                let v = value("WIDTHxHEIGHT");
                let (w, h) = v.split_once('x').expect("--size WIDTHxHEIGHT");
                a.size = (w.parse().expect("a width"), h.parse().expect("a height"));
            }
            "--latency" => {
                a.latency = Duration::from_millis(value("milliseconds").parse().expect("a number"))
            }
            "--swing" => a.swing = value("a speed").parse().expect("a number"),
            "--port" => a.port = value("a port").parse().expect("a port number"),
            "--no-reproject" => a.reproject = false,
            "--no-guest" => a.guest = false,
            "--seconds" => a.seconds = Some(value("seconds").parse().expect("a number")),
            "--no-depth" => a.depth = false,
            "--external" => a.external = true,
            other => panic!("unknown option {other}"),
        }
    }
    a
}

// ---------------------------------------------------------------------------------------------
// The guest: a thread that speaks the link and renders on the CPU

static STOP: AtomicBool = AtomicBool::new(false);

/// The pixel file's largest frame; the guest renders as large as fits in it with the host's aspect ratio.
const GUEST_MAX: (u32, u32) = (640, 360);

/// A ray-cast sphere in the host's world.
struct Sphere {
    centre: Vec3,
    radius: f32,
    /// Base colour; the shading is a fixed light and the surface normal
    colour: Vec3,
}

/// What the guest shows at guest time `t`: a big sphere sunk into the wall and a small one circling it in
/// front of the wall (sinking into the ground and passing behind the pillar on the way).
fn guest_spheres(t: f32) -> [Sphere; 2] {
    let centre = Vec3::new(-0.2, 1.2, -0.7);
    let a = t * 0.9 + PHASE;
    [
        Sphere {
            centre,
            radius: 1.3,
            colour: Vec3::new(0.95, 0.35, 0.55),
        },
        Sphere {
            centre: Vec3::new(centre.x + a.cos() * 2.0, centre.y + a.sin() * 2.0, 0.9),
            radius: 0.4,
            colour: Vec3::new(0.35, 0.9, 0.5),
        },
    ]
}

/// Where the small sphere starts on its circle, radians
const PHASE: f32 = 0.9;

/// Renders the spheres as seen by `cam`: straight-alpha RGBA8 and linear depth in metres (the distance
/// along the camera's forward axis, not to the eye), alpha 0 and `+inf` where nothing is.
fn render_guest(cam: &HostCamera, (w, h): (u32, u32), t: f32) -> (Vec<u8>, Vec<f32>) {
    let view = Mat4::from_cols_array(&cam.view);
    let eye = view.inverse().transform_point3(Vec3::ZERO);
    // view space -> world directions
    let to_world = view.inverse();
    let spheres = guest_spheres(t);
    let light = Vec3::new(-0.4, 0.8, 0.5).normalize();
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let mut depth = vec![f32::INFINITY; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let nx = (x as f32 + 0.5) / w as f32 * 2.0 - 1.0;
            let ny = 1.0 - (y as f32 + 0.5) / h as f32 * 2.0;
            // The view-space direction with z = -1, so the ray parameter is the linear depth
            let dir =
                to_world.transform_vector3(Vec3::new(nx / cam.proj[0], ny / cam.proj[5], -1.0));
            let mut best: Option<(f32, Vec3)> = None;
            for s in &spheres {
                let oc = eye - s.centre;
                let a = dir.length_squared();
                let b = 2.0 * oc.dot(dir);
                let c = oc.length_squared() - s.radius * s.radius;
                let disc = b * b - 4.0 * a * c;
                if disc < 0.0 {
                    continue;
                }
                let hit = (-b - disc.sqrt()) / (2.0 * a);
                if hit > cam.near && best.is_none_or(|(d, _)| hit < d) {
                    let n = (eye + dir * hit - s.centre) / s.radius;
                    let shade = 0.35 + 0.65 * n.dot(light).max(0.0);
                    best = Some((hit, s.colour * shade));
                }
            }
            if let Some((d, c)) = best {
                let i = (y * w + x) as usize;
                // sRGB-encoded bytes, as the pixel file carries them
                let enc = |v: f32| (v.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0).round() as u8;
                rgba[i * 4..i * 4 + 4].copy_from_slice(&[enc(c.x), enc(c.y), enc(c.z), 255]);
                depth[i] = d;
            }
        }
    }
    (rgba, depth)
}

/// The size to render for a host viewport: as large as fits in the pixel file, in the viewport's shape.
fn frame_size(viewport: [u32; 2]) -> (u32, u32) {
    let aspect = viewport[0].max(1) as f32 / viewport[1].max(1) as f32;
    let (mw, mh) = (GUEST_MAX.0 as f32, GUEST_MAX.1 as f32);
    let (w, h) = if aspect >= mw / mh {
        (mw, (mw / aspect).round())
    } else {
        ((mh * aspect).round(), mh)
    };
    (w.max(8.0) as u32, h.max(8.0) as u32)
}

struct Pending {
    due: Instant,
    rgba: Vec<u8>,
    depth: Vec<f32>,
    size: (u32, u32),
    camera_seq: u32,
    time_us: u64,
}

fn run_guest(port: u16, latency: Duration, with_depth: bool) {
    let mut link = match Link::guest(LinkConfig {
        control_port: port,
        ..LinkConfig::new("Synthetic guest")
    }) {
        Ok(link) => link,
        Err(e) => {
            eprintln!("guest: cannot start: {e}");
            return;
        }
    };
    let start = Instant::now();
    let mut writer: Option<FrameWriter> = None;
    let mut pending: VecDeque<Pending> = VecDeque::new();
    while !STOP.load(Ordering::Relaxed) {
        let mut newest: Option<(u32, HostCamera)> = None;
        for event in link.poll() {
            match event {
                Event::Connected(peer) => {
                    let tag = format!("guest-{:08x}", peer.session);
                    let mut opts = FrameFileOptions::new(GUEST_MAX.0, GUEST_MAX.1, 3);
                    if with_depth {
                        opts = opts.with_depth();
                    }
                    match FrameWriter::create_temp_with(&tag, &opts) {
                        Ok(w) => {
                            let _ = link.announce_frames(w.channel());
                            writer = Some(w);
                        }
                        Err(e) => eprintln!("guest: no pixel file: {e}"),
                    }
                }
                Event::Lost { .. } => {
                    writer = None;
                    pending.clear();
                }
                Event::Camera { header, camera } => newest = Some((header.seq, camera)),
                _ => {}
            }
        }
        // Only the newest camera of a batch is worth rendering
        if let (Some((seq, camera)), Some(_)) = (newest, &writer) {
            let size = frame_size(camera.viewport);
            let t = start.elapsed().as_secs_f32();
            let (rgba, depth) = render_guest(&camera, size, t);
            pending.push_back(Pending {
                due: Instant::now() + latency,
                rgba,
                depth,
                size,
                camera_seq: seq,
                time_us: now_us(),
            });
        }
        while pending.front().is_some_and(|p| p.due <= Instant::now()) {
            let p = pending.pop_front().unwrap();
            if let Some(w) = &mut writer {
                let depth = w.has_depth().then_some(p.depth.as_slice());
                if let Ok(ready) =
                    w.write_depth(&p.rgba, depth, p.size.0, p.size.1, p.camera_seq, p.time_us)
                {
                    let _ = link.send_frame(&ready);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    // `writer` drops here and deletes the pixel file
}

/// The guest thread's handle, so `main` can wait for it (and the pixel file's cleanup) after the app ends.
#[derive(Resource, Clone, Default)]
struct GuestThread(Arc<Mutex<Option<JoinHandle<()>>>>);

fn start_guest(host: Option<NonSend<HostLink>>, args: Res<Args>, thread: Res<GuestThread>) {
    if args.external || !args.guest {
        return;
    }
    let Some(addr) = host.and_then(|h| h.link.control_addr()) else {
        return;
    };
    let (latency, depth) = (args.latency, args.depth);
    *thread.0.lock().unwrap() = Some(std::thread::spawn(move || {
        run_guest(addr.port(), latency, depth)
    }));
}

// ---------------------------------------------------------------------------------------------
// The host: a small Bevy scene

#[derive(Component)]
struct Swing;

#[derive(Resource)]
struct ScreenshotTarget(Handle<Image>);

/// Off screen the swing is counted in frames from the moment the guest's frames start to arrive, so every
/// run (with or without reprojection) shows the camera at the same phase when the picture is taken.
#[derive(Resource, Default)]
struct SwingClock {
    frames: u32,
    running: bool,
}

/// The swing's phase at which the off-screen picture is taken, seconds: the camera is a little left of
/// centre and moving fast, so a late frame is easy to see.
const SHOT_AT: f32 = 4.13;

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    args: Res<Args>,
) {
    let mut solid = |c: Color| materials.add(StandardMaterial::from(c));
    // The ground, a wall the guest's big sphere stands half inside of, a pillar in front of the sphere's
    // right side, a crate near the camera
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(40.0, 40.0))),
        MeshMaterial3d(solid(Color::srgb(0.22, 0.27, 0.22))),
    ));
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(3.4, 2.4, 0.2))),
        MeshMaterial3d(solid(Color::srgb(0.78, 0.72, 0.58))),
        Transform::from_xyz(-1.9, 1.2, 0.0),
    ));
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(0.7, 2.8, 0.7))),
        MeshMaterial3d(solid(Color::srgb(0.2, 0.35, 0.85))),
        Transform::from_xyz(0.45, 1.4, 1.5),
    ));
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(0.7, 0.7, 0.7))),
        MeshMaterial3d(solid(Color::srgb(0.95, 0.5, 0.15))),
        Transform::from_xyz(-1.0, 0.35, 3.6).with_rotation(Quat::from_rotation_y(0.5)),
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 9000.0,
            ..default()
        },
        Transform::from_xyz(-3.0, 6.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    commands.insert_resource(GlobalAmbientLight {
        brightness: 500.0,
        ..default()
    });

    let mut camera = commands.spawn((
        Camera3d::default(),
        // What the compositing needs of a camera: no MSAA (its depth buffer has one sample per pixel), no HDR
        Msaa::Off,
        Tonemapping::None,
        DebandDither::Disabled,
        Camera {
            clear_color: ClearColorConfig::Custom(Color::srgb(0.45, 0.6, 0.8)),
            ..default()
        },
        Transform::from_xyz(0.0, 2.2, 7.5).looking_at(Vec3::new(-0.3, 1.0, 0.0), Vec3::Y),
        GuestCompose {
            reproject: args.reproject,
        },
        Swing,
    ));
    if args.screenshot.is_some() {
        let mut target = Image::new_target_texture(
            args.size.0,
            args.size.1,
            TextureFormat::Rgba8UnormSrgb,
            None,
        );
        target.asset_usage = RenderAssetUsages::RENDER_WORLD;
        let target = images.add(target);
        camera.insert(RenderTarget::from(target.clone()));
        commands.insert_resource(ScreenshotTarget(target));
    }
}

/// The camera swings from side to side around the scene, by frame count when off screen so a run is the
/// same every time
fn swing(
    time: Res<Time>,
    clock: Res<SwingClock>,
    args: Res<Args>,
    mut cameras: Query<&mut Transform, With<Swing>>,
) {
    let t = if args.screenshot.is_some() {
        clock.frames as f32 / 60.0
    } else {
        time.elapsed_secs()
    };
    let yaw = (t * args.swing).sin() * 0.55;
    for mut tf in &mut cameras {
        let eye = Quat::from_rotation_y(yaw) * Vec3::new(0.0, 0.0, 7.5) + Vec3::new(0.0, 2.2, 0.0);
        *tf = Transform::from_translation(eye).looking_at(Vec3::new(-0.3, 1.0, 0.0), Vec3::Y);
    }
}

/// Once a second: what the link and the plugin did; also quits after `--seconds`
fn report(
    time: Res<Time>,
    args: Res<Args>,
    mut exit: MessageWriter<AppExit>,
    host: Option<NonSend<HostLink>>,
    frames: Res<GuestFrames>,
    mut last: Local<(f32, u64, u64)>,
    mut fps: Local<u32>,
) {
    if args.seconds.is_some_and(|s| time.elapsed_secs() > s) {
        exit.write(AppExit::Success);
    }
    *fps += 1;
    if time.elapsed_secs() - last.0 < 1.0 {
        return;
    }
    let Some(host) = host else { return };
    let stats = host.stats();
    let age = frames.frame().map(|f| f.age().as_secs_f32() * 1000.0);
    info!(
        "{} fps; guest {}; {} cameras sent, {} frames shown ({} missed) this second; newest frame {} ms old; camera {}",
        *fps,
        if stats.connected { "connected" } else { "not connected" },
        stats.cameras_sent - last.1,
        stats.frames_shown - last.2,
        stats.frames_missed,
        age.map_or("-".into(), |a| format!("{a:.0}")),
        frames
            .frame()
            .map_or("-".into(), |f| format!("seq {} in the ring: {}", f.camera_seq, frames.render_camera().is_some())),
    );
    *last = (time.elapsed_secs(), stats.cameras_sent, stats.frames_shown);
    *fps = 0;
}

/// Off screen: waits for the guest's frames to flow, lets the camera swing to `SHOT_AT`, takes the picture
fn take_screenshot(
    mut commands: Commands,
    mut clock: ResMut<SwingClock>,
    mut waited: Local<u32>,
    host: Option<NonSend<HostLink>>,
    args: Res<Args>,
    target: Option<Res<ScreenshotTarget>>,
    mut exit: MessageWriter<AppExit>,
) {
    let (Some(target), Some(path)) = (target, args.screenshot.clone()) else {
        return;
    };
    *waited += 1;
    let flowing = host.is_some_and(|h| h.stats().frames_shown >= 20) || !args.guest;
    if !clock.running {
        if flowing && *waited > 30 {
            clock.running = true;
        } else if *waited > 900 {
            error!("no guest frames after 15 s; giving up");
            exit.write(AppExit::error());
        }
        return;
    }
    clock.frames += 1;
    let shot = (SHOT_AT * 60.0) as u32;
    if clock.frames == shot {
        commands
            .spawn(Screenshot::image(target.0.clone()))
            .observe(save_to_disk(path));
    } else if clock.frames == shot + 30 {
        exit.write(AppExit::Success);
    }
}

#[cfg(feature = "window")]
fn keys_and_title(
    keys: Res<ButtonInput<KeyCode>>,
    mut cameras: Query<&mut GuestCompose, With<Swing>>,
    mut windows: Query<&mut Window>,
) {
    for mut settings in &mut cameras {
        if keys.just_pressed(KeyCode::KeyR) {
            settings.reproject = !settings.reproject;
        }
        for mut window in &mut windows {
            window.title = format!(
                "rewrite-bridge-bevy guest_compose | reprojection (R): {}",
                if settings.reproject { "on" } else { "off" }
            );
        }
    }
}

fn main() {
    let args = parse_args();
    let thread = GuestThread::default();
    let mut app = App::new();
    app.insert_resource(args.clone())
        .insert_resource(SwingClock::default())
        .insert_resource(thread.clone());
    if args.screenshot.is_some() {
        // Off screen: no window, and a loop that paces itself at 60 Hz
        app.add_plugins((
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: None,
                    exit_condition: ExitCondition::DontExit,
                    ..default()
                })
                .set(bevy::render::RenderPlugin {
                    synchronous_pipeline_compilation: true,
                    ..default()
                }),
            ScheduleRunnerPlugin::run_loop(Duration::from_micros(16_667)),
        ));
    } else {
        #[cfg(feature = "window")]
        app.add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                resolution: args.size.into(),
                title: "rewrite-bridge-bevy guest_compose".into(),
                ..default()
            }),
            ..default()
        }));
        #[cfg(not(feature = "window"))]
        panic!("without --screenshot this needs a window: build with --features compose,window");
    }
    let port = if args.external { args.port } else { 0 };
    app.add_plugins((
        GuestComposePlugin::default(),
        HostLinkPlugin::new(LinkConfig {
            control_port: port,
            ..LinkConfig::new("Bevy host example")
        }),
    ))
    .add_systems(Startup, (setup, start_guest))
    .add_systems(Update, (swing, report, take_screenshot));
    #[cfg(feature = "window")]
    app.add_systems(Update, keys_and_title);
    app.run();

    STOP.store(true, Ordering::Relaxed);
    let handle = thread.0.lock().unwrap().take();
    if let Some(handle) = handle {
        let _ = handle.join();
    }
}
