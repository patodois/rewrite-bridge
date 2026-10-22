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

## License

Licensed under either of MIT or Apache-2.0, at your option.
