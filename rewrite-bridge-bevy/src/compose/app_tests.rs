//! The plugin inside a real (headless) Bevy app: a host scene of unlit boxes, a guest frame that is put
//! straight into [`GuestFrames`], and the camera's picture read back. It checks the wiring the shader
//! tests cannot: that the depth buffer of a `Camera3d` is readable, that the pass runs after the main
//! pass and writes the view target, that sRGB targets round-trip, and that the nearer of the two wins
//! in each direction.
//!
//! Needs the `viewer` feature next to `compose` (Bevy's window-less default plugins and PNG). Without
//! a GPU adapter it prints `SKIP` and passes.

use std::sync::{Arc, Mutex};

use bevy::asset::RenderAssetUsages;
use bevy::camera::RenderTarget;
use bevy::core_pipeline::tonemapping::{DebandDither, Tonemapping};
use bevy::prelude::*;
use bevy::render::render_resource::TextureFormat;
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured};
use bevy::render::RenderPlugin;
use bevy::window::ExitCondition;

use super::{GuestCompose, GuestComposePlugin, GuestFrame, GuestFrames};

const SIZE: (u32, u32) = (160, 120);

#[derive(Resource)]
struct Target(Handle<Image>);

/// Where the camera stands and whether the host scene is spawned.
#[derive(Resource, Clone, Copy)]
struct Setup {
    camera: Transform,
    scene: bool,
    reproject: bool,
}

fn unlit(materials: &mut Assets<StandardMaterial>, c: Color) -> MeshMaterial3d<StandardMaterial> {
    MeshMaterial3d(materials.add(StandardMaterial {
        base_color: c,
        unlit: true,
        ..default()
    }))
}

fn setup(
    setup: Res<Setup>,
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mut target = Image::new_target_texture(SIZE.0, SIZE.1, TextureFormat::Rgba8UnormSrgb, None);
    target.asset_usage = RenderAssetUsages::RENDER_WORLD;
    let target = images.add(target);
    commands.insert_resource(Target(target.clone()));
    commands.spawn((
        Camera3d::default(),
        RenderTarget::from(target),
        Msaa::Off,
        Tonemapping::None,
        DebandDither::Disabled,
        Camera {
            clear_color: ClearColorConfig::Custom(Color::srgb_u8(20, 20, 20)),
            ..default()
        },
        setup.camera,
        GuestCompose {
            reproject: setup.reproject,
        },
    ));
    if !setup.scene {
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
fn guest() -> GuestFrame {
    let (w, h) = SIZE;
    let mut rgba = Vec::new();
    let mut depth = Vec::new();
    for _y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&[255, 0, 0, 255]);
            depth.push(if x < w / 2 { 3.0 } else { 8.0 });
        }
    }
    GuestFrame::new(1, 0, 0, SIZE, rgba, Some(depth)).unwrap()
}

/// Runs the app with `camera`, lets `after_start` put frames in, and returns the camera's picture, or `None`
/// without a GPU. `after_start` also gets to read what the camera looks like by then.
fn render(
    camera: Transform,
    scene: bool,
    reproject: bool,
    after_start: impl FnOnce(&mut World),
) -> Option<Image> {
    super::tests::gpu()?;
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
    .insert_resource(Setup {
        camera,
        scene,
        reproject,
    })
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
    let captured: Arc<Mutex<Option<Image>>> = Arc::default();
    let target = app.world().resource::<Target>().0.clone();
    let sink = captured.clone();
    app.world_mut().spawn(Screenshot::image(target)).observe(
        move |event: On<ScreenshotCaptured>| {
            *sink.lock().unwrap() = Some(event.image.clone());
        },
    );
    for _ in 0..30 {
        app.update();
        if captured.lock().unwrap().is_some() {
            break;
        }
    }
    let image = captured.lock().unwrap().take();
    image
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

    let run = |reproject_on: bool| {
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
            let frame = GuestFrame::new(1, 0, 5, SIZE, rgba, Some(depth)).unwrap();
            let mut frames = world.resource_mut::<GuestFrames>();
            frames.push_camera(5, render_cam);
            frames.push_camera(6, present);
            frames.set_frame(frame.clone());
            *seen.lock().unwrap() = Some((render_cam, present, frame));
        })
    };
    let Some(late) = run(true) else {
        eprintln!("SKIP: no GPU adapter");
        return;
    };
    let as_is = run(false).expect("a picture");
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
}
