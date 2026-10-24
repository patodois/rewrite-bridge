# rewrite-bridge-bevy

[![crates.io](https://img.shields.io/crates/v/rewrite-bridge-bevy.svg)](https://crates.io/crates/rewrite-bridge-bevy)
[![docs.rs](https://img.shields.io/docsrs/rewrite-bridge-bevy)](https://docs.rs/rewrite-bridge-bevy)

A [Bevy](https://bevy.org) 0.20 plugin for [rewrite-bridge](https://github.com/patodois/rewrite-bridge):
load the models, skins, animations and scenes of any game rewrite's `Provider` into a Bevy game in a few
lines. It knows nothing about any game; it spawns what the bridge's types describe.

```toml
[dependencies]
bevy = "0.20"
rewrite-bridge = "0.2"
rewrite-bridge-bevy = "0.2"
```

Both come from crates.io, so every provider you add shares the one copy of rewrite-bridge that this plugin
uses, as long as it asks for the same `0.2` line (see
[one copy for the whole build](https://github.com/patodois/rewrite-bridge#one-copy-for-the-whole-build)).

```rust
use bevy::prelude::*;
use rewrite_bridge::Provider;
use rewrite_bridge_bevy::*;

#[derive(Resource)]
struct Game(Box<dyn Provider + Send + Sync>);

fn main() {
    App::new()
        .add_plugins((DefaultPlugins, RewriteBridgePlugin::default()))
        .insert_resource(Game(open_provider()))
        .add_systems(Startup, setup)
        .add_systems(Update, play)
        .run();
}

fn setup(mut bridge: Bridge, game: Res<Game>) {
    let robot = bridge.spawn_animated(game.0.load_animated("robot").unwrap());
    let yard = bridge.spawn_scene(game.0.load_scene("yard").unwrap(), &Providers::new(game.0.as_ref())).unwrap();
    // `robot.root` and `yard.root` are the entities to move; both convert into `Entity`
}

fn play(mut players: Query<(&BridgeAnimations, &mut AnimationPlayer), Added<BridgeAnimations>>) {
    for (clips, mut player) in &mut players {
        clips.play(&mut player, "walk").unwrap().repeat();
    }
}
```

## API

| | |
|---|---|
| `RewriteBridgePlugin` | adds Bevy's animation plugin if missing and keeps the `SpawnOptions` resource |
| `Bridge` (system parameter) | `spawn_model(Model)`, `spawn_animated(AnimatedModel)`, `spawn_scene(Scene, &dyn ModelSource)`; fields `commands`, `assets`, `animation`, `options` are public |
| `spawn_model` / `spawn_model_owned` | the same over `Commands` and `BridgeAssets`, copying or moving the model |
| `spawn_animated` / `spawn_animated_owned` | model, clips, graph, player and sockets |
| `spawn_scene` | instances, lights, sky; asset references go through a `ModelSource` |
| `Providers`, `NoSource`, closures | the sources: `Providers::new(&local).with(&other)` routes `AssetRef`s by game name |
| `to_animation_clip`, `target_id`, `to_bevy_mesh`, `to_bevy_image`, `to_standard_material`, `prepare_parts`, `rest_bounds`, ... | the conversions, for when you want the pieces |

What gets spawned:

- **Model**: a root entity (`BridgeModel`), one entity per node (`BridgeNode(i)`, rest transform), a `Mesh3d`
  per mesh (normals, tangents, UVs; missing normals computed, missing tangents generated for normal-mapped
  materials), one `StandardMaterial` per material shared by its meshes, and a `SkinnedMesh` per skinned mesh
  whose joints are the node entities. Pose a model by changing the node `Transform`s.
- **Animated model**: the above, plus each `Animation` as an `AnimationClip` (one curve per channel; linear,
  slerp, step and cubic spline; the clip lasts `duration_s`), an `AnimationGraph` with a node per clip, an
  `AnimationPlayer` on the root and `AnimationTargetId` + `AnimatedBy` on every node. `BridgeAnimations`
  finds clips by name: `clips.play(&mut player, "walk")`. Each `Socket` is a named child of its node
  (`BridgeSocket`, also in the root's `BridgeSockets` map): parent a weapon to it and it follows every clip.
  `emissive_strength` is folded into the emissive factor.
- **Scene**: a root (`BridgeScene`) and one entity per `Instance` (`BridgeInstance`). Every instance of a
  scene model shares that model's mesh and material handles, so Bevy batches them; models are uploaded once,
  and not at all if nothing uses them. A model that fails to load is listed in `SpawnedScene::failed` and its
  instances are skipped. Lights are children (`BridgeLight(i)`). `SpawnedScene::sky` goes on a camera with
  `sky.apply(&mut commands, camera)`.

## Conventions and units

The bridge follows glTF 2.0 and so does Bevy (metres, +Y up, right-handed, counter-clockwise front faces,
quaternions as x, y, z, w, OpenGL normal maps, roughness in G and metalness in B), so no axis or handedness
conversion happens.

| bridge | Bevy |
|---|---|
| directional light, lux | `DirectionalLight::illuminance`, unchanged |
| point light, candela | `PointLight::intensity`, lumens = candela x 4π |
| spot light, candela | `SpotLight::intensity`, lumens = candela x 4π whatever the cone (as Bevy's glTF loader) |
| light range `None` | `SpawnOptions::unbounded_light_range` (1000 m) |
| emissive | `emissive_factor * emissive_strength * emissive_scale` (times the emissive image), `emissive_scale` = 1 |
| flat sky colour | the camera's clear colour (not exposed: 1.0 is white) and an ambient light of `colour * environment_scale` cd/m² |
| equirectangular sky | a cube map for `Skybox` and `GeneratedEnvironmentMapLight` at `intensity * environment_scale` cd/m² |

**Exposure.** Lights are photometric, and Bevy exposes them with the camera's `Exposure`. This crate does not
pick an exposure or invent a scale: choose the `Exposure` for your lights (`Exposure::SUNLIGHT` for a day
scene, `Exposure::INDOOR` for lamps). The bridge's relative values (emissive, sky; 1.0 is reference white)
are handled like this:

- emissive colours are not touched by exposure in Bevy by default (`emissive_exposure_weight` 0), so a factor
  of 1.0 is white under any exposure and `emissive_scale` stays 1.0, as in Bevy's own glTF loader. If you set
  `emissive_exposure_weight` to 1, set `emissive_scale` to `white_luminance(exposure)` too;
- the sky is lit through the exposure: set `SpawnOptions::environment_scale` to `white_luminance(your_exposure)`,
  which is `1.2 * 2^ev100` cd/m². The default is that of Bevy's default exposure (about 995).

**Mirrored placements.** A negative scale mirrors, which flips the winding of triangles on screen, and Bevy
does not account for that when culling. Instances (and rigid meshes on nodes) that mirror therefore get a
copy of their material with front-face culling, as Bevy's glTF loader does, made once per material. A pose
that animates a node into a mirror is not followed.

## Depth compositing of a guest's frames (feature `compose`)

A passthrough guest (docs/CONTRACT.md, section 9) can send RGBA frames with a depth plane in linear metres.
With the `compose` feature this crate puts them into a Bevy camera's image: the guest's pixel wins where its
distance is smaller than the camera's own depth buffer says, the camera's pixel wins where it is nearer, and
a frame that arrives late is first reprojected from the camera it was rendered with to the camera the view
presents now. It is the GPU form of the core crate's CPU reference, `rewrite_bridge::link::compose`
(contract section 9.7), with the same rules and tested against it.

```toml
rewrite-bridge-bevy = { version = "0.2", features = ["compose"] }
```

```rust
use bevy::prelude::*;
use rewrite_bridge::link::LinkConfig;
use rewrite_bridge_bevy::compose::*;

App::new()
    .add_plugins((
        DefaultPlugins,
        GuestComposePlugin::default(),
        // optional: listens for a guest, sends it the camera, reads its frames
        HostLinkPlugin::new(LinkConfig::new("My Game")),
    ))
    .add_systems(Startup, |mut commands: Commands| {
        commands.spawn((Camera3d::default(), Msaa::Off, GuestCompose::default()));
    })
    .run();
```

| | |
|---|---|
| `GuestComposePlugin` | the render passes and the `GuestFrames` resource |
| `GuestCompose` (component) | on a `Camera3d`: composite the guest's frames into it; `reproject: false` skips the warp |
| `GuestFrames` (resource) | the newest `GuestFrame` and the ring of host cameras (`push_camera(seq, camera)`, `set_frame(frame)`), for an app that runs its own `Link` |
| `HostLinkPlugin` | `Link::host` as a plugin: polls it, reads the pixel file the guest announces, and sends the `GuestCompose` camera once per frame, remembered under the `seq` the link gives it |
| `host_camera` | a Bevy camera as the contract's `Camera` (view, projection, near, far, reversed depth) |

How a frame gets onto the screen: the guest's colour (`Rgba8Uint`, the bytes as the file carries them) and
depth (`R32Float`) are uploaded once per frame. After the main pass, three compute passes reproject them to
the view (a splat of each source pixel's warped square, the nearest surface wins, ties go to the lowest
source index, exactly as the reference loops), and one fullscreen pass composites: it converts the camera's
depth buffer to metres from the camera's own projection, tests it against the guest's depth, and blends with
the contract's straight-alpha "over" in the sRGB-encoded 8-bit values, with the reference's integer
arithmetic (an `*Srgb` target is converted to encoded values and back around the blend). A frame the size of
a different view is resampled with the reference's rule for frames it could not match to a camera; a frame
without a depth plane is pasted over the image, as the contract says, and is never warped.

What the camera has to be: a perspective projection, `Msaa::Off`, and not `Hdr`. Anything else is left alone
with a warning (see the module docs of `rewrite_bridge_bevy::compose` for why). Only what is in the depth
buffer takes part, so alpha-blended host materials neither hide the guest nor are hidden by it, and TAA
jitter is not accounted for.

**Verified against the CPU reference.** `cargo test -p rewrite-bridge-bevy --features compose,viewer`
renders the same scenes through `link::compose` and through the shaders and compares every pixel: the plain
composite (both occlusion directions, translucent guest pixels, translucent host pixels, a finite and an
infinite far plane, a viewport inside a larger target, a frame without depth) must match exactly and does;
through an sRGB target it may be off by one step and is not; reprojection (a sideways move, a move forward and
a turn, a lunge at the ground, a finite far plane, frames 1.7, 5 and 20 times smaller than the view) may differ
at footprint edges in at most 0.05% of the pixels, because the shaders run in `f32` where the reference is
`f64`, and on the development GPU (Apple M5, Metal) it differs in none of them except for 6 pixels (0.031%) in
the 20 times stretch. A test inside a headless Bevy app checks the wiring (host boxes hiding and covering a
guest layer, a late frame reprojected to the camera the real `ExtractedView` has). Without a GPU adapter the
tests print `SKIP`.

**Cost.** One frame's upload, reprojection and composite, waiting for the GPU to finish it, in a release
build on the same machine: 0.7 ms for a 640x360 guest frame into a 1280x720 view, 1.0 ms into 1920x1080, 2.0 ms
for a 1280x720 frame into 1920x1080 or a 960x540 one into 2560x1440. The CPU reference takes 10 ms for the
first of these. (`cargo test --release -p rewrite-bridge-bevy --features compose --lib frame_times -- --ignored --nocapture`.)

**Example.** `examples/guest_compose.rs` is a Bevy scene (ground, wall, pillar, crate) with the plugins above
and a guest that is a thread of the same program: it speaks the link, ray-casts two spheres with the host's
camera on the CPU and sends colour and depth, one sphere sunk into the wall and one that circles, sinks into
the ground and passes behind the pillar. No game files.

```sh
cargo run --release --example guest_compose --features compose,window            # a window; R toggles reprojection
cargo run --release --example guest_compose --features compose,viewer -- --screenshot out.png --latency 250
# `--no-reproject`, `--no-depth`, `--no-guest`, `--swing 1.5`, `--size 1280x720`: see the top of the file
# another guest instead of the built-in one (`bridge-guest`, or rewrite-bridge's `fake_guest --frames`):
cargo run --release --example guest_compose --features compose,window -- --external
```

## Memory

`spawn_model_owned`, `spawn_animated_owned`, `spawn_scene` and `prepare_parts` take the model by value and move
its vertices and pixels into Bevy's assets instead of copying them, and everything is uploaded with
`RenderAssetUsages::RENDER_WORLD` (change `SpawnOptions::usage` to keep CPU copies), so the CPU side frees as
the GPU takes over. Scenes move one model at a time. Mip chains are built on the CPU (box filter in linear
light for sRGB) and add about a third to an image; turn them off with `SpawnOptions::mipmaps`.

## Bevy features

Bevy is a dependency with default features off, asking only for `std`, `bevy_asset`, `bevy_log`,
`bevy_color`, `bevy_image`, `bevy_mesh`, `bevy_mikktspace`, `bevy_animation`, `bevy_curve`, `bevy_camera`,
`bevy_light`, `bevy_render` and `bevy_pbr` (the `compose` feature adds `bevy_core_pipeline`, for the render
schedule it adds a pass to, and turns on `rewrite-bridge`'s `link` feature). No audio, no gamepad, no windowing: your own `bevy` dependency adds
what your game wants, so on Linux this crate needs no `libasound2-dev` or `libudev-dev` (Bevy's `DefaultPlugins`
with its default features does).

## Example

`examples/viewer.rs` uses a synthetic provider that generates a robot (rigid and skinned meshes, an emissive
visor, two clips, a socket with a tool in it), a crate prop and a scene of instanced trees and crates with a
sun, a lamp, a torch and an equirectangular sky. No game files.

```sh
# Off screen (no window or display needed); writes a PNG and exits
cargo run --release --example viewer --features viewer -- --screenshot out.png
# Options: --size 1280x720  --clip wave|idle  --at SECONDS  --no-sky  --view EX,EY,EZ,TX,TY,TZ
# In a window, orbiting the scene
cargo run --release --example viewer --features window
```

## Tests

With `--features compose` (and `viewer`, for the test that runs a whole headless app) the compositing tests
above run too. `cargo test -p rewrite-bridge-bevy` covers meshes, materials, mip chains, the node hierarchy and skins, the
conversion of every channel kind to Bevy curves, a clip played by name through Bevy's animation systems,
instancing (shared handles, mirrored materials, lazily loaded assets, failures), light units and the sky's
cube map orientation.

## License

Licensed under either of MIT or Apache-2.0, at your option.
