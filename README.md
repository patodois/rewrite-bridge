# rewrite-bridge

rewrite-bridge is for people writing game **rewrites** (new engines that load the original game's
assets from the player's own install, like IW4L) and **passthrough or mash-up mods** (two running
games that share one world). It gives them one set of model, animation and scene types, a `Provider`
trait that each game's reader implements, and a small protocol for linking two running games.

- A **game rewrite** runs an existing game on a new engine. It ships no game data: it reads the files
  of the copy the player already owns. A reader for one game's formats is called a *provider* here.
- A **passthrough** (or **mash-up**) puts one game's things into another, like the mod that lets a
  character from one game walk through the levels of another. Either the assets are loaded into the
  second game through a provider, or two games run side by side and one draws what the other sends.

```
 Assets: a provider turns one game's files into common types, any renderer can draw them

   +-----------------+       +------------------------+       +--------------------------+
   | game files      |       | provider               |       | consumer                 |
   | (the player's   | ----> | `impl Provider` for    | ----> | your rewrite's engine,   |
   | own install)    |  read | one game: list, load   | Model | rewrite-bridge-bevy,     |
   +-----------------+       +------------------------+ Scene | bridge-testbed           |
                                                              +--------------------------+

 Passthrough: two running games on one machine (the `link` feature, docs/CONTRACT.md)

   +-----------------------+   camera, collision    +-----------------------+
   | host: the game the    | ---------------------> | guest: a second game  |
   | player is in          | <--------------------- | or a rewrite          |
   +-----------------------+  poses, spawns, frames +-----------------------+
```

**New here?** To read a game's files into the bridge, start with
[**Writing a provider**](docs/WRITING_A_PROVIDER.md), which has a complete example you can run. To
draw what a provider returns, see [Using it in Bevy](#using-it-in-bevy). To link two running games,
see [Passthrough link](#passthrough-link). Unfamiliar words (node, skin, socket, seqlock ...) are in
the [glossary](#glossary).

## Projects using it

| Project | What it is |
|---|---|
| [wt-unpack](https://github.com/patodois/wt-unpack) | Provider for War Thunder: vehicles with their rigs, and levels. |
| [wf-unpack](https://github.com/patodois/wf-unpack) | Provider for Warframe: models, animations and levels, with level collision. |
| [hytale-unpack](https://github.com/patodois/hytale-unpack) | Provider for Hytale: entity models, animations, blocks and prefabs. |
| [nocturne-unpack](https://github.com/patodois/nocturne-unpack) | Provider for Shin Megami Tensei III Nocturne HD Remaster: demons with their animations, and fields. |
| [bridge-testbed](https://github.com/patodois/bridge-testbed) | The visual and physics testbed: loads items from every provider side by side. |
| Host plugins for Hytale and Minecraft | Not published yet. |

Add yours with a pull request or an issue once it is public.

## Using it

```toml
[dependencies]
rewrite-bridge = { git = "https://github.com/patodois/rewrite-bridge", tag = "v0.1.0" }
```

### One tag for the whole build

> **Every crate in one build must use the same rewrite-bridge tag.** Cargo treats a different `tag`,
> `rev` or branch as a different crate, even when the code is the same. A provider built against one
> and a renderer built against another then each have their own `Provider` and `Model`, and neither
> accepts the other's:
>
> ```text
> error[E0277]: the trait bound `Game: Provider` is not satisfied
> note: there are multiple different versions of crate `rewrite_bridge` in the dependency graph
> ```
>
> Check with `cargo tree -i rewrite-bridge`. With one version it prints one tree whose children are
> every crate that depends on it. With two it refuses:
>
> ```text
> error: specification `rewrite-bridge` is ambiguous
> help: re-run this command with one of the following specifications
>   git+https://github.com/patodois/rewrite-bridge?tag=v0.1.0#0.1.0
>   git+https://github.com/patodois/rewrite-bridge?rev=0c5d0b0e08fcff58166f2ed6ad3daf9bdc72c0a8#0.1.0
> ```
>
> Fix it by moving every dependency to the same tag. `rewrite-bridge-bevy` comes from this same
> repository, so give it the same `tag` too.

### Rust and build times

- **Rust.** Edition 2021, stable. Everything here is built and tested with rustc 1.99.1. The core
  crate calls `slice::as_chunks`, stable since 1.88, so 1.88 is the least that can build it; no
  compiler older than 1.99.1 has been tried, so that is a floor read from the standard library's
  documentation, not a tested minimum. `rewrite-bridge-bevy` needs what Bevy 0.20 needs: its
  manifest declares Rust 1.97.1. CI uses the latest stable.
- **First build, clean `target`, on an Apple M5 with 10 cores and 16 GB, four jobs while other builds
  were running, with the dependencies already downloaded** (a cold download adds the time of a
  network fetch). These are rough, yours will differ:

  | what | about |
  |---|---|
  | the core crate (`cargo build`) | 2 s |
  | with the `link` feature and the examples (`--release`) | 5 s |
  | `cargo build -p rewrite-bridge-bevy` | 1 minute |
  | the Bevy viewer example, debug, on top of that | 45 s |
  | the Bevy viewer example in release (`--release`), from clean | 3.5 minutes, one compiler process peaking at about 1.7 GB |

## Conventions

Each game's provider implements `Provider`, which lists assets and loads them as a `Model`: a node
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

Implement `Provider` for a type that holds the path to the player's install:

```rust
use rewrite_bridge::{AssetInfo, BoxError, Model, Provider};

struct MyGame { /* path to the player's install */ }

impl Provider for MyGame {
    fn game(&self) -> &str { "My Game" }
    fn list(&self, filter: &str) -> Result<Vec<AssetInfo>, BoxError> { todo!("your game's assets") }
    fn load(&self, id: &str) -> Result<Model, BoxError> {
        let model: Model = todo!("convert your format, flipping axes and units to the conventions above");
        model.validate()?;
        Ok(model)
    }
}
```

Here the `todo!`s stand for your game. [`examples/cube_provider.rs`](examples/cube_provider.rs) is the
same provider with them filled in, for a made-up game whose one asset is a textured cube: a `Mesh`
with positions, normals, UVs and indices, a `Material` with an image, and a `Model` that passes
`validate` and the [conformance checker](#checking-your-provider) with no findings. Run it with
`cargo run --example cube_provider`. The
[guide to writing a provider](docs/WRITING_A_PROVIDER.md) goes from there to skins, clips, levels,
memory and publishing, with what four real providers do about each.

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
rewrite-bridge = { git = "https://github.com/patodois/rewrite-bridge", tag = "v0.1.0" }
rewrite-bridge-bevy = { git = "https://github.com/patodois/rewrite-bridge", tag = "v0.1.0" }
```

Use the same tag for both, and for every provider you add (see [one tag](#one-tag-for-the-whole-build)).

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

[`docs/CONTRACT.md`](docs/CONTRACT.md) is the versioned protocol (v1, revision 4), with every byte offset and C
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

The [examples](#examples) run a fake host and a fake guest against each other, with no game.

Without the feature the crate still has no dependencies; with it, it uses `memmap2` and
`serde_json`. The link only exchanges what each process publishes about itself: it never reads
another process's memory or injects code.

## Examples

All are in [`examples/`](examples). None needs game files.

| example | what it is |
|---|---|
| `cube_provider` | A complete provider in one file: the example of the [guide](docs/WRITING_A_PROVIDER.md). Exits 0. |
| `conformance` | The conformance checker run on a toy provider with one good and one deliberately wrong asset. |
| `fake_host`, `fake_guest` | A host and a guest for the passthrough link, to see it work. Need `--features link`. |

### A provider and the checker

```sh
cargo run --example cube_provider          # one asset, no findings, exit status 0
cargo run --example conformance            # exits 1 on purpose, see below
cargo run --example conformance -- --id right    # the good asset only: exits 0
```

`conformance` exits with status 1 **on purpose**: its toy provider has an asset called `wrong` (a cube
in centimetres read as metres, with its triangles wound backwards), so the checker reports an error
and a warning and exits 1, the way it would in a script or in CI. That is the demonstration, not a
failure of the build. `--id right` checks only the good asset and exits 0, which is what CI runs.

### The passthrough link

Two terminals. **Start the host first** (a guest that starts first retries every half second until a
host answers, but the host's output is easier to read from the start). Release builds matter: both
render on the CPU.

```sh
# terminal 1: the host listens on 127.0.0.1:47800 and draws a pillar and a floor
cargo run --release --features link --example fake_host -- --seconds 10 --dump target/frames
# terminal 2: the guest connects, and renders a lit cube with the host's camera
cargo run --release --features link --example fake_guest -- --frames --seconds 10
```

- **Stopping.** Both run until you press Ctrl-C by default (`--seconds` defaults to 0, which means no
  limit); `--seconds 10` makes each stop after ten seconds.
- **`--frames` on the guest** makes it send pictures (colour and depth). Without it the guest only
  sends poses and the host has nothing to composite or dump.
- **`--dump DIR` on the host** writes a few frames as PPM files (`P6`, 320 x 240, about 225 KB each).
  `target/frames` is a good place, because `target/` is already ignored by git. By default it writes
  every 25th presented frame, six times, in three variants of the same moment, so 18 files, named
  by variant and the number of the frame:
  - `composite_` is what the host presents: the guest's frame reprojected to the host's newest
    camera and depth-tested against the host's own pixels, so the pillar hides part of the cube;
  - `pasted_` is the same guest frame pasted over the host's image with no depth test and no
    reprojection, so the cube is in front of the pillar: what a plain overlay would give;
  - `stale_` is depth-tested but not reprojected, so the cube is placed as it was rendered, for an
    older camera: it shows the lag the reprojection removes.

  `--dump-every N` and `--dump-max N` change how often and how many. PPM is simple, but many viewers
  do not open it. On macOS, convert them with the built-in `sips`; elsewhere use ImageMagick:

  ```sh
  cd target/frames
  for f in *.ppm; do sips -s format png "$f" --out "${f%.ppm}.png"; done    # macOS
  magick mogrify -format png *.ppm                                         # ImageMagick 7 (6: mogrify -format png *.ppm)
  ```
- **`--in-front` on the guest** is for a real game's host. By default the cube stands at the world
  origin, which is where `fake_host` expects it. With `--in-front` it stands 4 m ahead of the first
  camera the host sends, on the ground the host reports as collision, because a real game's player is
  rarely near the origin. Host plugins for real games are not published yet; any host that speaks
  [`docs/CONTRACT.md`](docs/CONTRACT.md) will do, in any language.

## Glossary

- **Node**: a transform (translation, rotation, scale) in a model's hierarchy, relative to its
  parent. Parents always come before their children. Meshes hang on nodes, and a skeleton's bones are
  nodes.
- **Mesh**: a triangle list with one material: positions, and optionally normals, tangents, UVs and,
  for a skinned mesh, joints and weights.
- **Skin**: what makes a mesh bend. A list of joints (nodes the mesh follows) with an inverse bind
  matrix for each, and per vertex up to four joints and their weights.
- **Socket**: a named attachment point (a hand, a muzzle): a node plus an offset from it, so it
  follows the node through every animation.
- **Clip**: one animation (`Animation` in the code): a name, a duration and channels that move the
  translation, rotation or scale of nodes over time.
- **Scene**: a level: models placed many times, with optional lights and an environment.
- **Instance**: one placement of a model in a scene: the model's index and a transform. A forest is
  one tree model and ten thousand instances.
- **Provider**: a type that implements `Provider` for one game: it reads the player's install and
  lists and loads assets as the types above.
- **`AssetKind`**: what a listed asset is: `Vehicle`, `Character`, `Weapon`, `Prop`, `Level` or
  `Other`. A `Level` is loaded as a scene, the rest as models.
- **Seqlock**: a way to share a buffer between a writer and readers in two processes without a
  lock. The writer makes a counter odd before writing and even after; a reader that sees the same
  even counter before and after copying knows its copy is whole. The link's pixel file uses one.
- **Reversed-Z**: a depth buffer convention in which the near plane has depth 1 and the far plane
  0 (Bevy and many Direct3D engines use it, often with an infinite far plane, because it spends float
  precision better). The link's depth plane is linear metres, so it does not depend on it; a host
  converts its own buffer.

## License

Licensed under either of MIT or Apache-2.0, at your option.
