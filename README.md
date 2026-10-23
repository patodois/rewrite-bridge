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
[below](#animation-scenes-and-attachment-points).

The crate has no dependencies and reads no game files. Providers read the player's own
installation, and no game data is ever part of this project.

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

## Passthrough link

With the optional `link` feature, two processes on the same machine can run as one mash-up: a
**host** (the game the player is in) sends its camera and the collision near the player, and a
**guest** answers with entity poses, spawns that name a provider's asset, and optionally rendered
RGBA frames. Input focus is handed back and forth, and a heartbeat watchdog notices when either
side freezes or dies.

- Control: JSON lines over TCP on `127.0.0.1` (default port 47800).
- State: little-endian binary UDP datagrams on `127.0.0.1`, at most 8192 bytes each.
- Frames: a memory-mapped file in the temp directory, guarded by a seqlock.
- Coordinates: the same glTF conventions as `Model`; each side converts its own.

[`docs/CONTRACT.md`](docs/CONTRACT.md) is the versioned protocol (v1), with every byte offset and C
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

Try it with two terminals:

```sh
cargo run --features link --example fake_host
cargo run --features link --example fake_guest -- --frames
```

Without the feature the crate still has no dependencies; with it, it uses `memmap2` and
`serde_json`. The link only exchanges what each process publishes about itself: it never reads
another process's memory or injects code.

## License

Licensed under either of MIT or Apache-2.0, at your option.
