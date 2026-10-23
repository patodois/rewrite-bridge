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
its own mistakes before a renderer does.

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
