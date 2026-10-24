//! The plugin inside a real (headless) Bevy app: a host scene of unlit boxes, a guest frame that is put
//! straight into [`GuestFrames`], and the camera's picture read back. It checks the wiring the shader
//! tests cannot: that the depth buffer of a `Camera3d` is readable, that the pass runs after the main
//! pass and writes the view target, that sRGB targets round-trip, and that the nearer of the two wins
//! in each direction.
//!
//! Needs the `viewer` feature next to `compose` (Bevy's window-less default plugins and PNG). Without
//! a GPU adapter it prints `SKIP` and passes.

use std::sync::{Arc, Mutex};

use bevy::anti_alias::fxaa::Fxaa;
use bevy::asset::RenderAssetUsages;
use bevy::camera::{Hdr, RenderTarget, Viewport};
use bevy::core_pipeline::tonemapping::{DebandDither, Tonemapping};
use bevy::prelude::*;
use bevy::render::render_resource::TextureFormat;
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured};
use bevy::render::RenderPlugin;
use bevy::window::ExitCondition;

use rewrite_bridge::link::LinkConfig;

use super::{GuestCompose, GuestComposePlugin, GuestFrame, GuestFrames, HostLinkPlugin};

const SIZE: (u32, u32) = (160, 120);

/// One camera of a scenario, with its own render target of `SIZE`.
#[derive(Clone)]
struct Cam {
    transform: Transform,
    reproject: bool,
    msaa: Msaa,
    hdr: bool,
    fxaa: bool,
    /// Position and size of the camera's viewport in its target
    viewport: Option<(UVec2, UVec2)>,
    order: isize,
}

impl Default for Cam {
    fn default() -> Self {
        Self {
            transform: Transform::default(),
            reproject: true,
            msaa: Msaa::Off,
            hdr: false,
            fxaa: false,
            viewport: None,
            order: 0,
        }
    }
}

#[derive(Resource, Clone)]
struct Scenario {
    scene: bool,
    cams: Vec<Cam>,
}

#[derive(Resource, Default)]
struct Spawned {
    targets: Vec<Handle<Image>>,
    cameras: Vec<Entity>,
}

fn unlit(materials: &mut Assets<StandardMaterial>, c: Color) -> MeshMaterial3d<StandardMaterial> {
    MeshMaterial3d(materials.add(StandardMaterial {
        base_color: c,
        unlit: true,
        ..default()
    }))
}

fn setup(
    scenario: Res<Scenario>,
    mut spawned: ResMut<Spawned>,
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for cam in &scenario.cams {
        let mut target =
            Image::new_target_texture(SIZE.0, SIZE.1, TextureFormat::Rgba8UnormSrgb, None);
        target.asset_usage = RenderAssetUsages::RENDER_WORLD;
        let target = images.add(target);
        let mut entity = commands.spawn((
            Camera3d::default(),
            RenderTarget::from(target.clone()),
            cam.msaa,
            Tonemapping::None,
            DebandDither::Disabled,
            Camera {
                clear_color: ClearColorConfig::Custom(Color::srgb_u8(20, 20, 20)),
                order: cam.order,
                viewport: cam.viewport.map(|(position, size)| Viewport {
                    physical_position: position,
                    physical_size: size,
                    ..default()
                }),
                ..default()
            },
            cam.transform,
            GuestCompose {
                reproject: cam.reproject,
            },
        ));
        if cam.hdr {
            entity.insert(Hdr);
        }
        if cam.fxaa {
            entity.insert(Fxaa::default());
        }
        spawned.cameras.push(entity.id());
        spawned.targets.push(target);
    }
    if !scenario.scene {
        return;
    }
    // The wall, 5 m ahead, whose top edge is above the middle of the picture; pillars 4 m and 2.5 m ahead.
    let cube = meshes.add(Cuboid::new(1.0, 1.0, 1.0));
    for (pos, scale, color) in [
        (
            Vec3::new(0.0, -2.5, -5.0),
            Vec3::new(12.0, 6.0, 0.2),
            Color::srgb_u8(30, 200, 60),
        ),
        (
            Vec3::new(-1.8, 0.0, -4.0),
            Vec3::new(0.8, 5.0, 0.8),
            Color::srgb_u8(230, 200, 30),
        ),
        (
            Vec3::new(0.9, 0.0, -2.5),
            Vec3::new(0.8, 5.0, 0.8),
            Color::srgb_u8(40, 70, 230),
        ),
    ] {
        commands.spawn((
            Mesh3d(cube.clone()),
            unlit(&mut materials, color),
            Transform::from_translation(pos).with_scale(scale),
        ));
    }
}

/// A frame that is red everywhere, 3 m away on the left half and 8 m on the right.
fn guest_of(size: (u32, u32)) -> GuestFrame {
    let (w, h) = size;
    let mut rgba = Vec::new();
    let mut depth = Vec::new();
    for _y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&[255, 0, 0, 255]);
            depth.push(if x < w / 2 { 3.0 } else { 8.0 });
        }
    }
    GuestFrame::new(1, 0, 0, size, rgba, Some(depth)).unwrap()
}

fn guest() -> GuestFrame {
    guest_of(SIZE)
}

/// What a run leaves: one picture per camera, in order, the cameras' entities and the one the plugin
/// chose.
struct Out {
    images: Vec<Image>,
    cameras: Vec<Entity>,
    chosen: Option<Entity>,
}

/// Two Bevy apps starting at once in one process trip over each other's shader loading
static ONE_APP_AT_A_TIME: Mutex<()> = Mutex::new(());

/// The app of a scenario, started, with the lock that keeps other apps out until it is dropped; `None`
/// without a GPU, and also when Bevy cannot open the adapter that the headless device could (that is a
/// runner's problem, not the plugin's: it is reported and the test skips, unless `RB_REQUIRE_GPU` is
/// set). Only the starting of the app is guarded that way: a panic once it runs is a real failure.
fn build_app(scenario: Scenario, link: bool) -> Option<(App, std::sync::MutexGuard<'static, ()>)> {
    super::tests::gpu()?;
    let lock = ONE_APP_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let skip = |e: Box<dyn std::any::Any + Send>| {
        let why = e
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        eprintln!("SKIP: Bevy could not start on this adapter: {why}");
        assert!(std::env::var_os("RB_REQUIRE_GPU").is_none(), "{why}");
    };
    let mut app = App::new();
    let added = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        app.add_plugins(
            // Not pipelined, so that a test can reach the render app and its schedules
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: None,
                    exit_condition: ExitCondition::DontExit,
                    ..default()
                })
                .set(RenderPlugin {
                    synchronous_pipeline_compilation: true,
                    ..default()
                })
                .disable::<bevy::render::pipelined_rendering::PipelinedRenderingPlugin>(),
        );
    }));
    if let Err(e) = added {
        skip(e);
        return None;
    }
    app.insert_resource(scenario)
        .init_resource::<Spawned>()
        .add_plugins(GuestComposePlugin::default())
        .add_systems(Startup, setup);
    if link {
        app.add_plugins(HostLinkPlugin::new(LinkConfig {
            control_port: 0,
            ..LinkConfig::new("app test host")
        }));
    }
    let finished = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        app.finish();
        app.cleanup();
    }));
    if let Err(e) = finished {
        skip(e);
        return None;
    }
    Some((app, lock))
}

/// Runs the app, lets `after_start` put frames in, and returns the pictures, or `None` without a GPU.
fn run_scenario(scenario: Scenario, after_start: impl FnOnce(&mut World)) -> Option<Out> {
    let (mut app, _one) = build_app(scenario, false)?;
    for _ in 0..4 {
        app.update();
    }
    after_start(app.world_mut());
    for _ in 0..4 {
        app.update();
    }
    let targets = app.world().resource::<Spawned>().targets.clone();
    let captured: Arc<Mutex<Vec<Option<Image>>>> = Arc::new(Mutex::new(vec![None; targets.len()]));
    for (i, target) in targets.into_iter().enumerate() {
        let sink = captured.clone();
        app.world_mut().spawn(Screenshot::image(target)).observe(
            move |event: On<ScreenshotCaptured>| {
                sink.lock().unwrap()[i] = Some(event.image.clone());
            },
        );
    }
    for _ in 0..30 {
        app.update();
        if captured.lock().unwrap().iter().all(Option::is_some) {
            break;
        }
    }
    let images = captured
        .lock()
        .unwrap()
        .iter_mut()
        .map(|i| i.take().expect("a picture"))
        .collect();
    Some(Out {
        images,
        cameras: app.world().resource::<Spawned>().cameras.clone(),
        chosen: app.world().resource::<GuestFrames>().compose_camera(),
    })
}

/// One camera, one picture.
fn render(
    camera: Transform,
    scene: bool,
    reproject: bool,
    after_start: impl FnOnce(&mut World),
) -> Option<Image> {
    run_scenario(
        Scenario {
            scene,
            cams: vec![Cam {
                transform: camera,
                reproject,
                ..default()
            }],
        },
        after_start,
    )
    .map(|mut out| out.images.remove(0))
}

fn pixel(image: &Image, x: u32, y: u32) -> [u8; 3] {
    let data = image.data.as_ref().expect("pixels");
    let i = ((y * image.width() + x) * 4) as usize;
    [data[i], data[i + 1], data[i + 2]]
}

fn near(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 2)
}

#[test]
fn the_guest_is_hidden_behind_host_geometry_and_covers_what_is_behind_it() {
    let Some(without) = render(Transform::default(), true, true, |_| {}) else {
        eprintln!("SKIP: no GPU adapter");
        return;
    };
    let with = render(Transform::default(), true, true, |world| {
        world.resource_mut::<GuestFrames>().set_frame(guest());
    })
    .expect("a picture");
    if let Ok(dir) = std::env::var("RB_COMPOSE_DUMP") {
        for (name, img) in [("host_only", &without), ("with_guest", &with)] {
            let _ = img
                .clone()
                .try_into_dynamic()
                .unwrap()
                .save(format!("{dir}/{name}.png"));
        }
    }
    let red = [255, 0, 0];
    let wall = [30, 200, 60];
    let blue = [40, 70, 230];
    let clear = [20, 20, 20];

    let yellow = [230, 200, 30];

    // Without a guest the picture is the host's own.
    assert!(
        near(pixel(&without, 60, 100), wall),
        "{:?}",
        pixel(&without, 60, 100)
    );
    assert!(
        near(pixel(&without, 130, 100), blue),
        "{:?}",
        pixel(&without, 130, 100)
    );
    assert!(
        near(pixel(&without, 15, 100), yellow),
        "{:?}",
        pixel(&without, 15, 100)
    );
    assert!(
        near(pixel(&without, 95, 20), clear),
        "{:?}",
        pixel(&without, 95, 20)
    );

    // Guest 3 m on the left, wall 5 m: the guest covers the wall.
    assert!(
        near(pixel(&with, 60, 100), red),
        "{:?}",
        pixel(&with, 60, 100)
    );
    // Guest 8 m on the right, wall 5 m: the wall hides the guest.
    assert!(
        near(pixel(&with, 95, 100), wall),
        "{:?}",
        pixel(&with, 95, 100)
    );
    // Guest 8 m, pillar 2.5 m: the pillar hides it.
    assert!(
        near(pixel(&with, 130, 100), blue),
        "{:?}",
        pixel(&with, 130, 100)
    );
    // Guest 3 m, the far pillar 4 m: the guest covers it.
    assert!(
        near(pixel(&with, 15, 100), red),
        "{:?}",
        pixel(&with, 15, 100)
    );
    // Where the host drew nothing the guest shows, near or far.
    assert!(
        near(pixel(&with, 95, 20), red),
        "{:?}",
        pixel(&with, 95, 20)
    );
    assert!(
        near(pixel(&with, 60, 20), red),
        "{:?}",
        pixel(&with, 60, 20)
    );
}

#[test]
fn a_late_frame_is_reprojected_to_the_camera_the_view_presents() {
    use bevy::camera::Camera as BevyCamera;
    use rewrite_bridge::link::compose::{composite, reproject};
    use rewrite_bridge::link::Camera as HostCamera;

    // The guest rendered with camera A (seq 5, in the ring); the view presents camera B, 35 cm to the side.
    let a = Transform::from_xyz(0.0, 1.5, 2.0).looking_at(Vec3::new(0.0, 1.0, -4.0), Vec3::Y);
    let b = Transform::from_xyz(0.35, 1.5, 2.0).looking_at(Vec3::new(0.2, 1.0, -4.0), Vec3::Y);
    type Seen = Arc<Mutex<Option<(HostCamera, HostCamera, GuestFrame)>>>;
    let seen: Seen = Arc::default();

    let run = |reproject_on: bool, with_depth: bool| {
        let seen = seen.clone();
        render(b, false, reproject_on, move |world| {
            let mut query =
                world.query_filtered::<(&GlobalTransform, &BevyCamera), With<GuestCompose>>();
            let (global, camera) = query.single(world).unwrap();
            let proj = camera.clip_from_view();
            let size = camera.physical_viewport_size().unwrap();
            let present = super::host_camera(global, proj, size).unwrap();
            let render_cam = super::host_camera(&GlobalTransform::from(a), proj, size).unwrap();
            let (rgba, depth) = super::tests::render_guest(&render_cam, SIZE, true);
            let frame = GuestFrame::new(1, 0, 5, SIZE, rgba, with_depth.then_some(depth)).unwrap();
            let mut frames = world.resource_mut::<GuestFrames>();
            frames.push_camera(5, render_cam);
            frames.push_camera(6, present);
            frames.set_frame(frame.clone());
            *seen.lock().unwrap() = Some((render_cam, present, frame));
        })
    };
    let Some(late) = run(true, true) else {
        eprintln!("SKIP: no GPU adapter");
        return;
    };
    let as_is = run(false, true).expect("a picture");
    let (render_cam, present, frame) = seen.lock().unwrap().take().unwrap();
    let depth: Vec<f32> = frame.depth.as_ref().unwrap().to_vec();
    let n = (SIZE.0 * SIZE.1) as usize;
    let host_rgba: Vec<u8> = [20u8, 20, 20, 255].repeat(n);
    let host_t = vec![f32::INFINITY; n];

    let reference = |warp: bool| {
        let (rgba, d) = if warp {
            reproject(
                &frame.rgba,
                &depth,
                SIZE.0,
                SIZE.1,
                &render_cam,
                &present,
                SIZE.0,
                SIZE.1,
            )
            .unwrap()
        } else {
            (frame.rgba.to_vec(), depth.clone())
        };
        composite(&host_rgba, &host_t, &rgba, &d).unwrap().rgba
    };
    let compare = |name: &str, image: &Image, expected: Vec<u8>| {
        let data = image.data.as_ref().unwrap();
        let off = data
            .chunks(4)
            .zip(expected.chunks(4))
            .filter(|(g, c)| (0..3).any(|k| g[k].abs_diff(c[k]) > 1))
            .count();
        eprintln!("{name}: {off} of {n} pixels differ by more than 1");
        // 0.05 % of the pixels may sit on the other side of a footprint edge (see tests.rs)
        assert!(
            off as f64 <= n as f64 * 0.0005,
            "{name}: {off} of {n} pixels differ"
        );
    };
    compare("reprojected", &late, reference(true));
    compare("composited as it came", &as_is, reference(false));
    // and the reprojection did move the picture
    assert_ne!(late.data, as_is.data);

    // A frame without depth cannot be warped (a pixel's position needs its distance), so it is pasted as
    // it came whatever the ring holds
    let pasted = run(true, false).expect("a picture");
    let expected = composite(&host_rgba, &host_t, &frame.rgba, &vec![1.0; n])
        .unwrap()
        .rgba;
    compare("pasted, no depth plane", &pasted, expected);
}

const RED: [u8; 3] = [255, 0, 0];
const WALL: [u8; 3] = [30, 200, 60];
const CLEAR: [u8; 3] = [20, 20, 20];

fn put_guest(world: &mut World) {
    world.resource_mut::<GuestFrames>().set_frame(guest());
}

#[test]
fn a_camera_with_msaa_or_hdr_is_left_alone() {
    // (60, 100) is where the guest (3 m) covers the wall (5 m): red when composited, wall green when not
    for (name, cam) in [
        (
            "MSAA",
            Cam {
                msaa: Msaa::Sample4,
                ..default()
            },
        ),
        (
            "HDR",
            Cam {
                hdr: true,
                ..default()
            },
        ),
    ] {
        let picture = |guest: bool| {
            run_scenario(
                Scenario {
                    scene: true,
                    cams: vec![cam.clone()],
                },
                move |world| {
                    if guest {
                        put_guest(world)
                    }
                },
            )
        };
        let Some(without) = picture(false) else {
            eprintln!("SKIP: no GPU adapter");
            return;
        };
        let with = picture(true).unwrap();
        // The camera still renders; the guest is just not in it
        let (a, b) = (&without.images[0], &with.images[0]);
        assert_eq!(a.data, b.data, "{name}: the guest was composited");
        assert!(
            pixel(b, 60, 100)[1] > 150 && pixel(b, 60, 100)[0] < 100,
            "{name}: the wall should show, got {:?}",
            pixel(b, 60, 100)
        );
    }
}

#[test]
fn a_viewport_that_does_not_start_at_the_corner_composites_in_place() {
    // A 100 x 80 viewport at (40, 20) of the 160 x 120 target, and a guest frame of the viewport's size
    let (origin, size) = (UVec2::new(40, 20), UVec2::new(100, 80));
    let cam = Cam {
        viewport: Some((origin, size)),
        ..default()
    };
    let picture = |guest: bool| {
        run_scenario(
            Scenario {
                scene: true,
                cams: vec![cam.clone()],
            },
            move |world| {
                if guest {
                    world
                        .resource_mut::<GuestFrames>()
                        .set_frame(guest_of((size.x, size.y)));
                }
            },
        )
    };
    let Some(without) = picture(false) else {
        eprintln!("SKIP: no GPU adapter");
        return;
    };
    let with = picture(true).unwrap();
    let (host, composed) = (&without.images[0], &with.images[0]);
    let (mut wrong, mut red_seen, mut host_seen) = (0, 0, 0);
    for y in 0..SIZE.1 {
        for x in 0..SIZE.0 {
            let (h, c) = (pixel(host, x, y), pixel(composed, x, y));
            let inside =
                x >= origin.x && x < origin.x + size.x && y >= origin.y && y < origin.y + size.y;
            let expected = if !inside {
                h
            } else if x - origin.x < size.x / 2 {
                // guest at 3 m: it covers everything but what is nearer, the blue pillar (2.5 m)
                if near(h, [40, 70, 230]) {
                    h
                } else {
                    RED
                }
            } else if near(h, CLEAR) {
                // guest at 8 m: it shows where the host drew nothing, and the host hides it elsewhere
                RED
            } else {
                h
            };
            if !near(c, expected) {
                wrong += 1;
            }
            red_seen += usize::from(inside && near(c, RED));
            host_seen += usize::from(inside && near(c, h) && !near(h, CLEAR) && !near(c, RED));
        }
    }
    assert!(
        red_seen > 1500 && host_seen > 800,
        "{red_seen} red, {host_seen} host pixels"
    );
    assert!(wrong <= 4, "{wrong} pixels are not what the rule says");
}

#[test]
fn of_several_cameras_one_composites_and_the_others_are_left_alone() {
    let a = Cam::default(); // spawned first, order 0
    let b = Cam {
        order: -1, // the lowest order wins, whatever the entity
        ..default()
    };
    let picture = |guest: bool| {
        run_scenario(
            Scenario {
                scene: true,
                cams: vec![a.clone(), b.clone()],
            },
            move |world| {
                if guest {
                    put_guest(world)
                }
            },
        )
    };
    let Some(without) = picture(false) else {
        eprintln!("SKIP: no GPU adapter");
        return;
    };
    let with = picture(true).unwrap();
    assert_eq!(with.chosen, Some(with.cameras[1]));
    // camera 0 shows the host only; camera 1 composited
    assert_eq!(with.images[0].data, without.images[0].data);
    assert!(near(pixel(&with.images[1], 60, 100), RED));
    assert!(near(pixel(&without.images[1], 60, 100), WALL));
}

/// Like [`guest`], but white (FXAA works on luma, and red and the wall's green have about the same) and the
/// 3 m / 8 m split is a slanted line, so the guest meets the wall in a staircase for FXAA to smooth.
fn slanted_guest() -> GuestFrame {
    let (w, h) = SIZE;
    let mut rgba = Vec::new();
    let mut depth = Vec::new();
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&[255, 255, 255, 255]);
            let edge = 50.0 + y as f32 * 0.37;
            depth.push(if (x as f32) < edge { 3.0 } else { 8.0 });
        }
    }
    GuestFrame::new(1, 0, 0, SIZE, rgba, Some(depth)).unwrap()
}

#[test]
fn the_guest_is_composited_before_the_anti_aliasing_pass() {
    // On the wall's rows the guest's white (3 m) meets the wall (5 m) along a slanted line. Composited
    // before FXAA, the pass smooths that staircase: pixels along it are neither pure white nor pure wall.
    // Composited after, the edge stays as hard as the guest's bytes.
    let picture = |fxaa: bool| {
        run_scenario(
            Scenario {
                scene: true,
                cams: vec![Cam { fxaa, ..default() }],
            },
            |world| {
                world
                    .resource_mut::<GuestFrames>()
                    .set_frame(slanted_guest());
            },
        )
    };
    let Some(plain) = picture(false) else {
        eprintln!("SKIP: no GPU adapter");
        return;
    };
    let smoothed = picture(true).unwrap();
    let blended = |img: &Image| {
        let mut n = 0;
        for y in 50..120u32 {
            for x in 50..100u32 {
                let p = pixel(img, x, y);
                if !near(p, [255, 255, 255]) && !near(p, WALL) && !near(p, [40, 70, 230]) {
                    n += 1;
                }
            }
        }
        n
    };
    assert_eq!(
        blended(&plain.images[0]),
        0,
        "without FXAA the edge is hard"
    );
    assert!(
        blended(&smoothed.images[0]) > 20,
        "FXAA did not touch the edge between the guest and the host: the guest was composited after it"
    );
}

/// Keys of the systems of `schedule` that are the function `f` (system names are only kept with Bevy's
/// `debug` feature, but a function system's type id is its function's).
fn systems_of<M, F: bevy::ecs::system::IntoSystem<(), (), M> + 'static>(
    schedule: &bevy::ecs::schedule::Schedule,
    f: F,
) -> Vec<bevy::ecs::schedule::SystemKey> {
    use bevy::ecs::system::System as _;
    let wanted = bevy::ecs::system::IntoSystem::into_system(f).system_type();
    schedule
        .systems()
        .expect("a schedule that has run")
        .filter(|(_, system)| system.system_type() == wanted)
        .map(|(key, _)| key)
        .collect()
}

fn one_camera_scenario() -> Scenario {
    Scenario {
        scene: true,
        cams: vec![Cam::default()],
    }
}

#[test]
fn the_pass_runs_in_early_post_process_ahead_of_tonemapping_and_the_anti_aliasing() {
    use bevy::core_pipeline::schedule::{Core3d, Core3dSystems};
    use bevy::ecs::schedule::SystemSet;
    use bevy::render::RenderApp;

    let Some((mut app, _one)) = build_app(one_camera_scenario(), false) else {
        eprintln!("SKIP: no GPU adapter");
        return;
    };
    for _ in 0..4 {
        app.update();
    }
    let render = app.get_sub_app_mut(RenderApp).expect("a render app");
    render.world_mut().schedule_scope(Core3d, |_, schedule| {
        let pass = systems_of(schedule, super::compose_pass);
        assert_eq!(
            pass.len(),
            1,
            "the compose pass is in the Core3d schedule once"
        );
        let in_set = |set: Core3dSystems| {
            schedule
                .graph()
                .systems_in_set(set.intern())
                .map(|systems| systems.contains(&pass[0]))
                .unwrap_or(false)
        };
        // The sets run Prepass, MainPass, EarlyPostProcess, PostProcess in that order, and tonemapping, FXAA,
        // SMAA and sharpening are in PostProcess: being in the early set is being ahead of all of them
        assert!(in_set(Core3dSystems::EarlyPostProcess));
        assert!(!in_set(Core3dSystems::PostProcess));
        assert!(!in_set(Core3dSystems::MainPass));
    });
}

#[test]
fn the_host_link_sends_the_camera_that_composites_and_remembers_it_in_a_real_app() {
    use bevy::ecs::schedule::{NodeId, SystemSet};
    use rewrite_bridge::link::{Camera as HostCamera, Event, Link};

    let low = Cam {
        order: -1,
        transform: Transform::from_xyz(1.0, 0.5, 2.0),
        viewport: Some((UVec2::new(10, 10), UVec2::new(100, 80))),
        ..default()
    };
    let scenario = Scenario {
        scene: false,
        cams: vec![Cam::default(), low],
    };
    let Some((mut app, _one)) = build_app(scenario, true) else {
        eprintln!("SKIP: no GPU adapter");
        return;
    };
    for _ in 0..4 {
        app.update();
    }
    let port = app
        .world()
        .non_send::<super::HostLink>()
        .link
        .control_addr()
        .unwrap()
        .port();
    let mut guest = Link::guest(LinkConfig {
        control_port: port,
        reconnect: std::time::Duration::from_millis(50),
        ..LinkConfig::new("app test guest")
    })
    .unwrap();
    let mut got: Vec<(u32, HostCamera)> = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while got.len() < 5 {
        assert!(
            std::time::Instant::now() < deadline,
            "the guest got {} cameras",
            got.len()
        );
        app.update();
        for event in guest.poll() {
            if let Event::Camera { header, camera } = event {
                got.push((header.seq, camera));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    let cameras = app.world().resource::<Spawned>().cameras.clone();
    let frames = app.world().resource::<GuestFrames>();
    assert_eq!(frames.compose_camera(), Some(cameras[1]));
    let place = *app.world().get::<GlobalTransform>(cameras[1]).unwrap();
    for (seq, camera) in &got {
        // the camera with the lowest order: its viewport (not the other's 160 x 120) and its place
        assert_eq!(camera.viewport, [100, 80]);
        assert_eq!(camera.view, super::view_matrix(&place));
        // remembered under the seq it went out with
        assert_eq!(frames.cameras().get(*seq), Some(camera));
    }

    // The camera is sent after the choice is made, and the choice runs once with both plugins present
    app.world_mut().schedule_scope(PostUpdate, |_, schedule| {
        let choose = systems_of(schedule, super::choose_compose_camera);
        let send = systems_of(schedule, super::host_link::send_camera);
        assert_eq!((choose.len(), send.len()), (1, 1));
        let set = super::ComposeCameraSystems.intern();
        let key = schedule.graph().system_sets.get_key(set).expect("the set");
        assert!(schedule
            .graph()
            .dependency()
            .graph()
            .contains_edge(NodeId::Set(key), NodeId::System(send[0])));
        assert!(schedule
            .graph()
            .systems_in_set(set)
            .unwrap()
            .contains(&choose[0]));
    });
}
