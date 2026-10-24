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

use super::{GuestCompose, GuestComposePlugin, GuestFrame, GuestFrames};

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

/// Runs the app, lets `after_start` put frames in, and returns the pictures, or `None` without a GPU.
fn run_scenario(scenario: Scenario, after_start: impl FnOnce(&mut World)) -> Option<Out> {
    super::tests::gpu()?;
    // Two Bevy apps starting at once in one process trip over each other's shader loading
    static ONE_APP_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _one = ONE_APP_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(WindowPlugin {
                primary_window: None,
                exit_condition: ExitCondition::DontExit,
                ..default()
            })
            .set(RenderPlugin {
                synchronous_pipeline_compilation: true,
                ..default()
            }),
    )
    .insert_resource(scenario)
    .init_resource::<Spawned>()
    .add_plugins(GuestComposePlugin::default())
    .add_systems(Startup, setup);
    app.finish();
    app.cleanup();
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
