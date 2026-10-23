# rewrite-bridge

Common model types so game rewrites can load each other's assets into one scene, like the mash-up
mods that put one game's characters into another.

Each game's reader implements `Provider`, which lists assets and loads them as a `Model`: a node
hierarchy, triangle meshes, metallic-roughness PBR materials and optional skins. Every `Model` follows
glTF 2.0's conventions, so a renderer never needs to know which game a model came from:

- units are metres;
- +Y is up, the space is right-handed, and a model's front faces +Z;
- front faces wind counter-clockwise;
- colour and emissive images are sRGB, the rest linear;
- metallic-roughness images keep roughness in G and metalness in B;
- normal maps are tangent space with green pointing up, OpenGL style.

`Model::validate` checks indices, attribute counts, skins and joint weights, so a provider can catch
its own mistakes before a renderer does. Animation, scenes and attachment points are described
[below](#animation-scenes-and-attachment-points). Whether the output also *means* the right thing
(metres, not centimetres; Y up; feet on the ground; faces wound the way the normals say; a clip that
does not spin a limb) is what the [conformance checker](#checking-your-provider) finds out.

The crate has no dependencies and reads no game files. Providers read the player's own
installation, and no game data is ever part of this project. Renderers get their own crates: the
Bevy plugin is [`rewrite-bridge-bevy`](rewrite-bridge-bevy), described [below](#using-it-in-bevy).

## Adding your game

```rust
use rewrite_bridge::{AssetInfo, AssetKind, BoxError, Model, Provider};

struct MyGame { /* path to the player's install */ }

impl Provider for MyGame {
    fn game(&self) -> &str { "My Game" }
    fn list(&self, filter: &str) -> Result<Vec<AssetInfo>, BoxError> { todo!() }
    fn load(&self, id: &str) -> Result<Model, BoxError> {
        let model = todo!("convert your format, flipping axes and units to the conventions above");
        model.validate()?;
        Ok(model)
    }
}
```

## Animation, scenes and attachment points

A static `Model` is not enough for a game: characters move, levels repeat the same tree ten
thousand times, and a weapon has to sit in a hand. These types are additive; a provider that only
implements `list` and `load` keeps working unchanged. All of it follows the same glTF conventions
as `Model`, plus: time is in seconds and quaternions are x, y, z, w.

- **`AnimatedModel { model, animations, sockets, emissive_strength }`** is a `Model` with the
  extras next to it. `Provider::load_animated` returns it; the default wraps `load` with no clips.
  The extras live beside the model, not inside it, because `Model`, `Mesh` and `Material` are
  built with struct literals in other crates and a new field would break them.
- **`Animation { name, duration_s, channels }`**: each `Channel { node, target, interpolation,
  times, values }` animates the translation, rotation or scale of one node (an index into
  `model.nodes`). `values` is a flat `f32` list like a glTF accessor (3 floats per keyframe for
  translation and scale, 4 for rotation, three times as many for `CubicSpline`: in-tangent,
  value, out-tangent). `Animation::validate(node_count)` checks node indices, strictly
  increasing times inside `duration_s`, value counts, and unit quaternions.
- **`Socket { name, node, offset }`** is a named attachment point: a node plus an offset from it,
  so it follows the node through every animation. It is a list rather than a "nodes named like
  this are sockets" convention, because a convention cannot tell a socket from a bone and the
  extra node would shift the indices that skins and clips already use.
- **`Scene { name, models, instances, lights, environment }`**: `models` are `SceneModel::Inline`
  (a whole `Model`) or `SceneModel::Asset(AssetRef)` (a provider's asset id, loaded lazily and
  once, even for the same game's trees used a thousand times); an `Instance` is a model index
  and a `Transform` (translation in metres, rotation quaternion, scale). `Provider::load_scene`
  returns one; the default returns a `NotSupported` error.
- **`Light`** (directional in lux, point and spot in candela, as in `KHR_lights_punctual`) and
  **`Environment`** (a colour or an equirectangular picture) are optional: fill them in only if
  your game really has them.
- **Emissive units.** `Material::emissive_factor` is relative scene-referred radiance, 1.0 being
  reference white and values above 1 the HDR range; it is not photometric, unlike lights. The
  optional `AnimatedModel::emissive_strength` (one entry per material, or empty for 1) is
  `KHR_materials_emissive_strength`: the light out is `emissive_factor * emissive_strength`.
  Multiplying it into `emissive_factor` yourself is equally valid.

`AnimatedModel::validate` and `Scene::validate` check all of the above, so a provider can catch
its mistakes before a renderer does.

```rust
use rewrite_bridge::{
    AnimatedModel, Animation, AssetInfo, AssetKind, AssetRef, BoxError, Channel, Instance,
    Interpolation, Model, Node, Provider, Scene, SceneModel, Socket, Target, Transform,
};

struct MyGame;

impl Provider for MyGame {
    fn game(&self) -> &str { "My Game" }
    fn list(&self, _filter: &str) -> Result<Vec<AssetInfo>, BoxError> {
        Ok(vec![AssetInfo { id: "robot".into(), name: "Robot".into(), kind: AssetKind::Character }])
    }
    // The plain model keeps working for consumers that know nothing about clips.
    fn load(&self, id: &str) -> Result<Model, BoxError> {
        Ok(self.load_animated(id)?.model)
    }
    fn load_animated(&self, _id: &str) -> Result<AnimatedModel, BoxError> {
        let mut robot = AnimatedModel::new(Model {
            name: "robot".into(),
            nodes: vec![Node::new("root", None), Node::new("hand", Some(0))],
            ..Model::default()
        });
        robot.animations.push(Animation {
            name: "reach".into(),
            duration_s: 1.0,
            channels: vec![Channel::new(
                1, Target::Translation, Interpolation::Linear,
                vec![0.0, 1.0],                     // seconds
                vec![0.0, 0.0, 0.0, 0.0, 0.5, 0.0], // x, y, z in metres per keyframe
            )],
        });
        robot.sockets.push(Socket::new("grip", 1, Transform::IDENTITY));
        robot.validate()?;
        Ok(robot)
    }
    fn load_scene(&self, _id: &str) -> Result<Scene, BoxError> {
        let scene = Scene {
            name: "yard".into(),
            models: vec![SceneModel::Asset(AssetRef::local("robot"))],
            // Two robots, one model: loaded once, drawn twice.
            instances: vec![
                Instance::new(0, Transform::from_translation([0.0, 0.0, 0.0])),
                Instance::new(0, Transform::from_translation([3.0, 0.0, 0.0])),
            ],
            ..Scene::default()
        };
        scene.validate()?;
        Ok(scene)
    }
}
```

## Checking your provider

`validate` says a model is consistent; it cannot say that a tank is 100 times too large, that its
triangles wind the wrong way, or that a clip flips a quaternion's sign and spins an arm. The
`conformance` module goes beyond `validate` (it calls it first) and reports findings, each with a
severity (`error`, `warning`, `info`), a stable code such as `scale.too_large`, a message and a
location (mesh, node, material, clip, instance). It has no dependencies. The module docs list every
code and why it matters to a consumer.

```rust
use rewrite_bridge::conformance::{check_animated, check_model, check_provider, check_scene, Expect};

// One value. `Expect` is what you know about it; the default is generic.
let report = check_model(&model, &Expect::human());      // a person: 1.4 to 2.5 m tall, a character
let report = check_animated(&animated, &Expect::tank()); // 2 to 4 m tall, 4 to 10 m long
let report = check_scene(&scene, &Expect::default());
println!("{report}");                                    // findings, worst first
assert!(report.passed());                                // no errors (warnings are allowed)

// Everything a provider lists, one asset in memory at a time.
for (id, report) in check_provider(&my_game, &[] as &[&str], &[]) {
    assert!(report.passed(), "{id}: {report}");
}
```

What it checks: units and scale (the world bounding box of the default pose; `Expect` adds height
and length ranges), orientation of characters (Y up, feet near y = 0), geometry (NaN, unit normals,
triangle winding against the normals, degenerate triangles), skins (weights, joints, invertible and
consistent inverse bind matrices), materials and images (factor ranges, alpha mode against the
texture's alpha, normal-map sanity), clips (quaternion hemispheres, zero scales, root-motion speed in
m/s, a skeleton that stretches because the clip uses other units) and scenes (extent, instance scales,
asset references). The front is only judged where an asset names it (a node or socket called `muzzle`,
`eye`, `nose`, `headlight`, `front`, `tail`, `rear`); otherwise the facing is reported as unknown, never guessed. The colour
space of images is not checked because `Image` does not carry one: the contract fixes it.

Some findings are conventions, not bugs. Drop a code your game knowingly differs on with
`Expect::default().allowing("skin.bind_pose_mismatch")`, or `--allow` on the command line.

### A `check-conformance` subcommand

A provider crate that has a command line adds this in a few lines, and its users can then check their
own installation:

```rust
fn main() {
    let provider = MyGame::open(/* the player's install */);
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("check-conformance") {
        std::process::exit(rewrite_bridge::conformance::cli::run(&provider, args));
    }
    // ... the crate's other subcommands
}
```

```sh
my-game-tool check-conformance                      # every listed asset
my-game-tool check-conformance --id tiger --id t34  # these assets
my-game-tool check-conformance --kind character -v  # one kind, with info findings
my-game-tool check-conformance --filter tank --limit 20 --strict
```

Assets of kind `Level` are loaded as scenes and the rest with `load_animated`, one at a time. It
prints one block per asset, then a table of finding codes with counts and example ids, and exits 1
if any asset has an error (`--strict`: or a warning). `--compare-load` also requires `load(id)` to
equal `load_animated(id)?.model`, as the trait asks. A failed load or a panic in the provider is a
finding on that asset, not the end of the run. `cargo run --example conformance` shows the output on
a toy provider.

## Using it in Bevy

[`rewrite-bridge-bevy`](rewrite-bridge-bevy) is a Bevy 0.20 plugin that loads what any provider returns:
models with skins, animations with sockets, and scenes. A Rust game rewrite needs a few lines:

```toml
[dependencies]
rewrite-bridge = { git = "https://github.com/patodois/rewrite-bridge" }
rewrite-bridge-bevy = { git = "https://github.com/patodois/rewrite-bridge" }
```

```rust
use bevy::prelude::*;
use rewrite_bridge::Provider;
use rewrite_bridge_bevy::*;

fn setup(mut bridge: Bridge, game: Res<Game>) {   // `Game` wraps any `Provider`
    // A character: entities per node, skinned meshes, PBR materials, clips and sockets
    let robot = bridge.spawn_animated(game.0.load_animated("robot").unwrap());
    // A level: instances share one mesh and one material per model, lights are physical
    let yard = bridge.spawn_scene(
        game.0.load_scene("yard").unwrap(),
        &Providers::new(game.0.as_ref()), // loads the scene's `SceneModel::Asset` references
    ).unwrap();
}

fn play(mut players: Query<(&BridgeAnimations, &mut AnimationPlayer), Added<BridgeAnimations>>) {
    for (clips, mut player) in &mut players {
        clips.play(&mut player, "walk").unwrap().repeat();
    }
}
```

Add `RewriteBridgePlugin::default()` next to `DefaultPlugins`. Nothing is converted on the way in, as
Bevy follows glTF too. Lux and candela become Bevy's lux and lumens (candela times 4π), emissive is
`factor * strength`, and the sky goes through the camera's exposure; the
[crate's README](rewrite-bridge-bevy/README.md) has the formulas, the Bevy features it asks for (no audio,
no gamepad) and a viewer example that runs off screen with no game files:

```sh
cargo run --release -p rewrite-bridge-bevy --example viewer --features viewer -- --screenshot out.png
```

## Passthrough link

With the optional `link` feature, two processes on the same machine can run as one mash-up: a
**host** (the game the player is in) sends its camera and the collision near the player, and a
**guest** answers with entity poses, spawns that name a provider's asset, and optionally rendered
RGBA frames. Input focus is handed back and forth, and a heartbeat watchdog notices when either
side freezes or dies.

- Control: JSON lines over TCP on `127.0.0.1` (default port 47800).
- State: little-endian binary UDP datagrams on `127.0.0.1`, at most 8192 bytes each.
- Frames: a memory-mapped file in the temp directory, guarded by a seqlock. A frame is RGBA and
  can carry linear depth in metres, so the host can reproject it to its newest camera and let the
  nearer of its own pixel and the guest's win (`link::compose`, a CPU reference for testing and
  small images; engines do the same on the GPU).
- Coordinates: the same glTF conventions as `Model`; each side converts its own.

[`docs/CONTRACT.md`](docs/CONTRACT.md) is the versioned protocol (v1, revision 3), with every byte offset and C
structs, so plugins in other languages can implement it without this crate. In Rust:

```rust
use rewrite_bridge::link::{Event, Link, LinkConfig};

let mut link = Link::guest(LinkConfig::new("My Game"))?;
// Every frame or tick, e.g. in a Bevy system:
for event in link.poll() {            // never blocks
    if let Event::Camera { camera, .. } = event {
        // place your world relative to the host's camera, then link.send_poses(&[...])
    }
}
```

Try it with two terminals. The guest renders a lit cube on the CPU with the host's camera and
sends colour and depth; the host draws a pillar and a floor, reprojects the cube to its newest camera
and composites it, so the pillar cuts the cube. `--dump DIR` on the host writes a few frames as PPM:

```sh
cargo run --release --features link --example fake_host -- --dump /tmp/frames
cargo run --release --features link --example fake_guest -- --frames
```

Without the feature the crate still has no dependencies; with it, it uses `memmap2` and
`serde_json`. The link only exchanges what each process publishes about itself: it never reads
another process's memory or injects code.

## License

Licensed under either of MIT or Apache-2.0, at your option.
