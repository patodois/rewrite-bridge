# Writing a provider

A **provider** is the piece of code that reads one game's files and hands them over as
rewrite-bridge types, so that any renderer can draw them without knowing the game. This guide goes
from an empty crate to a published one, with a complete example you can run (section 5).

It was written from four real providers, and every statement about them was checked against the
code at these commits (the repositories keep moving, so the commit is the reference). The last column is the
first commit of each provider that takes rewrite-bridge from crates.io (`0.2.1`) instead of the `v0.1.0` git tag:

| crate | game | commit read | on crates.io from |
|---|---|---|---|
| [wt-unpack](https://github.com/patodois/wt-unpack) | War Thunder | `bc111a7` | `4915291` |
| [wf-unpack](https://github.com/patodois/wf-unpack) | Warframe | `d5c33e7` | `e3ec925` |
| [hytale-unpack](https://github.com/patodois/hytale-unpack) | Hytale | `9bbf066` | `dbc7cbe` |
| [nocturne-unpack](https://github.com/patodois/nocturne-unpack) | Shin Megami Tensei III Nocturne HD Remaster | `826b963` | `49df6cc` |

New words (node, skin, socket, seqlock ...) are in the [glossary](../README.md#glossary).

1. [What a provider is](#1-what-a-provider-is)
2. [The crate skeleton](#2-the-crate-skeleton)
3. [Find and open the install](#3-find-and-open-the-install)
4. [`list`](#4-list)
5. [`load` a static model: a complete example](#5-load-a-static-model-a-complete-example)
6. [Space and units](#6-space-and-units)
7. [Materials and textures](#7-materials-and-textures)
8. [Skins, animation and sockets](#8-skins-animation-and-sockets)
9. [Scenes](#9-scenes)
10. [Test with the checker](#10-test-with-the-checker)
11. [Memory and failure rules](#11-memory-and-failure-rules)
12. [See it](#12-see-it)
13. [Publish](#13-publish)
14. [Checklist](#14-checklist)

## 1. What a provider is

A provider is a Rust type that implements `rewrite_bridge::Provider` for one game. It

- **reads the player's own install** and ships no game data, ever;
- **lists** what the game has (`list`), and **loads** one asset by id as a `Model` (`load`), an
  `AnimatedModel` with clips and sockets (`load_animated`) or a `Scene`, which is a level
  (`load_scene`);
- **converts** the game's axes, units, textures and materials to the bridge's conventions, so
  that what comes out is the same kind of thing whichever game it came from.

The trait, abridged:

```rust
pub trait Provider {
    fn game(&self) -> &str;
    fn list(&self, filter: &str) -> Result<Vec<AssetInfo>, BoxError>;
    fn load(&self, id: &str) -> Result<Model, BoxError>;
    // These two have defaults, so a provider of static models implements only the three above.
    // The default of load_animated wraps `load` with no clips; the default of load_scene
    // returns the `NotSupported` error.
    fn load_animated(&self, id: &str) -> Result<AnimatedModel, BoxError> { /* ... */ }
    fn load_scene(&self, id: &str) -> Result<Scene, BoxError> { /* ... */ }
}
```

What consumes a provider: [`rewrite-bridge-bevy`](../rewrite-bridge-bevy) (spawns the result into a
Bevy app), a rewrite's own renderer, and [bridge-testbed](https://github.com/patodois/bridge-testbed)
(loads every provider side by side). None of them knows which game an asset came from, which is the
point.

The four existing providers, for orientation:

| crate | type, and how it opens | `list` gives | an id |
|---|---|---|---|
| wt-unpack | `wt_unpack::bridge::WarThunder::new(game_folder)` | vehicles (`Vehicle`), levels (`Level`) | `tanks/usa_m4a1_76w_sherman.grp:m4a1_76w_sherman`, `levels/avg_berlin.bin` |
| wf-unpack | `wf_unpack::bridge::Warframe::open(cache_windows)` | models (`Character`, `Weapon`, `Prop`), levels | `/Lotus/Characters/Tenno/Excalibur/Excalibur_skel.fbx`, a `.level` path |
| hytale-unpack | `hytale_unpack::bridge::Hytale::open(assets_zip)`, or `open_default()` | entity models, bare models, block items, prefabs | `Server/Models/Intelligent/Trork/Trork.json` |
| nocturne-unpack | `nocturne_unpack::bridge::Nocturne::open(game_folder)` | demons (`Character`), field prefabs (`Level`) | `dvl0x01`, `f012/f012_002` |

## 2. The crate skeleton

All four split the work the same way:

- a **library** with the readers for the game's formats (archives, models, textures), which knows
  nothing about the bridge;
- `src/bridge.rs` behind a cargo **feature** named `bridge`: the `Provider` and the conversions to
  bridge types;
- a **thin command line** (`src/main.rs`, clap) over the library, for listing, exporting and
  checking. The library is the provider; the command line is a convenience.

`Cargo.toml`, from hytale-unpack (the others make `bridge` opt-in instead of a default, so the
readers can be used without the bridge):

```toml
[dependencies]
# The bridge to other game rewrites
rewrite-bridge = { version = "0.2.1", optional = true }

[features]
default = ["bridge"]
bridge = ["dep:rewrite-bridge"]
```

> **One copy for the whole build.** Every crate in one build that depends on rewrite-bridge must
> end up with the same copy of it. Two copies are two crates that happen to share a name, and nothing
> from one is accepted by the other. The compiler's complaint is `E0277` with the note
> *there are multiple different versions of crate `rewrite_bridge` in the dependency graph*.
>
> From crates.io Cargo does this for you: `"0.2"`, `"0.2.1"` and `"^0.2.1"` all resolve to the same
> release, which is built once, so a provider that asks for `"0.2"` and a renderer that asks for `"0.2"`
> (or `"0.2.1"`) share it. Before 1.0 the minor number is the breaking one, so stay on one `0.x` line:
> a `"0.3"` in the build would be a second copy. A `git` dependency, a `path` or a `[patch]` is a
> different source and therefore a different copy too, which is why providers should depend on the
> crates.io release and not on a tag or a revision.
> `cargo tree -i rewrite-bridge` must list one entry, and `cargo tree -d` shows what is duplicated.
> The [README](../README.md#one-copy-for-the-whole-build) shows the symptom in full.

## 3. Find and open the install

The constructor takes the path of the install as an argument. Around it, a provider usually offers
three conveniences, and none of them belongs in the loading code:

- a **default location** for each OS the game runs on, which is the author's guess;
- an **environment variable** that overrides it;
- `~` **expansion**, because a command line's default value does not go through a shell.

hytale-unpack, `src/assets.rs` (it knows only the macOS launcher path; add the others your game has):

```rust
/// The default `Assets.zip` location: `$HYTALE_ASSETS` if set, else the macOS launcher path.
pub fn default_path() -> PathBuf {
    if let Some(p) = std::env::var_os("HYTALE_ASSETS") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(MAC_INSTALL)
}
```

nocturne-unpack, `src/install.rs`, expands the `~` inside the library, so `Nocturne::open("~/Games/smt3hd")`
works; wf-unpack expands it in its command line only, so a program that calls `Warframe::open` must
pass a real path. Either is fine if it is written down.

**Check the folder when you open it, and name the path in the error.** nocturne-unpack does:

```rust
.with_context(|| {
    format!(
        "{} holds no smt3hd_Data/StreamingAssets/PC folder (the one with the `PC` manifest)",
        game.display()
    )
})?;
```

and hytale-unpack's `Assets::open` says `opening <path>`. Without a check, a wrong folder can look
like a game with no assets: wt-unpack's `WarThunder::new` accepts any path, and `list` on a folder
that is not the game returns an empty list, not an error. Do better than that.

Other things a player needs, and a README should say:

- **Which files, and where they come from.** Some installs are Windows builds (the Nocturne Steam
  build, Warframe's `Cache.Windows`) that a player on another OS has to fetch some way; say which
  files you need and how to get them.
- **The tests' variable.** The four providers' game-data tests read `HYTALE_ASSETS`, `WT_GAME_DIR`,
  `WF_GAME_DIR` and `NOCTURNE_GAME_DIR` and skip when they are not set (section 10). It is easier on
  a user when the command line honours the same variable; nocturne-unpack's currently does not.
- **Cost of opening.** Read only an index when you open. hytale-unpack reads the zip's central
  directory (a few megabytes) of a 3.5 GB `Assets.zip` and seeks to single entries afterwards.

## 4. `list`

```rust
fn list(&self, filter: &str) -> Result<Vec<AssetInfo>, BoxError>;
// AssetInfo { id: String, name: String, kind: AssetKind }
```

**Ids come from the game's own paths, and stay stable.** An id is the only thing a user, a config
file or a scene's asset reference will ever hold, so derive it from what the game calls the asset
and do not number assets by their position in a listing. The four formats:

| provider | id | where it comes from |
|---|---|---|
| wt-unpack | `<pack>:<model>` for vehicles, `levels/<name>.bin` for levels | the `.grp` pack under `content/base/res` (a patched copy wins) and the model in it |
| wf-unpack | the asset path, `/Lotus/...` | the package's table of contents |
| hytale-unpack | the entry path in `Assets.zip`, exactly as the zip lists it | `Server/Models/**.json`, `Common/**.blockymodel`, `Server/Item/Items/**.json`, `Server/Prefabs/**.prefab.json` |
| nocturne-unpack | the bundle's file name; `bundle/prefab` for a prefab inside a field bundle | `dvl0x01`, `f012/f012_002` |

An id may carry options, if you say so. wf-unpack's ids can end in `?animations=FILTER` (clips,
section 8) or `?collision` (section 9). Such ids are not in `list`, so the checker needs
`--allow provider.unlisted_id` to accept them.

**List only what draws.** An id that loads into nothing is a trap for every consumer. All four
leave things out on purpose:

- hytale-unpack skips entity definitions that have no model (showcase mannequins that are only
  particles) and blocks whose `DrawType` is `Empty`;
- nocturne-unpack skips field prefabs that hold only transforms;
- wf-unpack lists a model as `Character` only if its rig is a whole body that stands on the ground
  (`is_whole_character`); helmets, cloth and sleeves are `Prop`;
- wt-unpack leaves out the damage and x-ray variants of a vehicle (`_dmg`, `_xray`, `_dm`) and
  models without a skeleton.

**`AssetKind` is a promise.** `Vehicle`, `Character`, `Weapon`, `Prop`, `Level`, `Other`.
`Level` means "load me with `load_scene`"; the checker does exactly that, and loads the others with
`load_animated`. `Character` makes the checker expect an upright figure with its feet on
y = 0 and between 0.2 and 6 m tall; the other kinds are not characters, so a skeleton in a vehicle
does not make it stand upright. `Other` asserts nothing. Choose by what the asset *is*, not by its
folder: a character's boots are not a `Character`.

**`filter`.** The trait says "id or name contains `filter`, everything for an empty filter". The
four differ in the details (hytale-unpack matches regardless of case, the others do not; wf-unpack
and wt-unpack look at the id only), so a user cannot rely on more than a plain substring. Case
insensitive, id or name, is the friendliest reading. The checker's `--filter` goes through your
`list`.

**Order and speed.** Return a stable, sorted order. Build the index once and keep it in the
provider (hytale-unpack uses a `OnceLock`, nocturne-unpack and wf-unpack an `Option` behind a
`Mutex`), because `list` is called once per filter and the checker calls it before anything else.

## 5. `load` a static model: a complete example

This is the whole of [`examples/cube_provider.rs`](../examples/cube_provider.rs): a provider for a
made-up game with one asset, a textured 1 m cube. It passes `Model::validate` and the conformance
checker with no findings at all, and CI builds it, runs it and tests it, so it cannot go stale.

```sh
cargo run --example cube_provider
```

```rust
//! A complete provider in one file: a made-up game whose only asset is a textured 1 m cube.
//!
//! It is the worked example of docs/WRITING_A_PROVIDER.md (section 5) and shows the four things
//! every provider does: list its assets, load one in the bridge's conventions, call `validate`
//! before returning it, and expose the conformance checker as a `check-conformance` subcommand.
//!
//! cargo run --example cube_provider                  # checks the one asset; exits 0
//! cargo run --example cube_provider -- -v            # also lists the info findings
//! cargo run --example cube_provider -- --help
//!
//! It reads no files: a real provider opens the player's install (see the guide, section 3).

use rewrite_bridge::{
    AssetInfo, AssetKind, BoxError, Image, Material, Mesh, Model, Node, Provider,
};

/// The id of the one asset. Real ids come from the game's own paths, so they stay stable.
const CUBE: &str = "props/cube";

struct CubeGame;

impl Provider for CubeGame {
    fn game(&self) -> &str {
        "Cube Game"
    }

    fn list(&self, filter: &str) -> Result<Vec<AssetInfo>, BoxError> {
        let all = [AssetInfo {
            id: CUBE.into(),
            name: "Cube".into(),
            kind: AssetKind::Prop,
        }];
        let filter = filter.to_ascii_lowercase();
        Ok(all
            .into_iter()
            .filter(|a| {
                a.id.to_ascii_lowercase().contains(&filter)
                    || a.name.to_ascii_lowercase().contains(&filter)
            })
            .collect())
    }

    fn load(&self, id: &str) -> Result<Model, BoxError> {
        if id != CUBE {
            return Err(format!("{}: no asset {id:?}", self.game()).into());
        }
        let model = cube();
        // Catch your own mistakes here, where the id and the data are at hand, not in a renderer.
        model.validate()?;
        Ok(model)
    }
}

/// A cube 1 m on a side, standing on y = 0 with its centre over the origin: one node, one mesh,
/// one material with a texture.
fn cube() -> Model {
    // One quad per face: its outward normal, and the two directions u and v along it, chosen so
    // that u x v = normal. Corners taken as (-1,-1), (1,-1), (1,1), (-1,1) in (u, v) then wind
    // counter-clockwise seen from outside, which is what the bridge wants.
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
    ];
    let mut mesh = Mesh {
        name: "cube".into(),
        node: 0,     // hangs on the "cube" node below
        material: 0, // the one material below
        ..Mesh::default()
    };
    for (n, u, v) in faces {
        let first = mesh.positions.len() as u32;
        for (a, b) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = [0, 1, 2].map(|k| 0.5 * (n[k] + a * u[k] + b * v[k]));
            // Metres, y up: the cube spans y = 0 to 1, so it stands on the ground.
            mesh.positions.push([p[0], p[1] + 0.5, p[2]]);
            mesh.normals.push(n);
            // glTF's UV origin is the image's top-left, so v runs down the image.
            mesh.uvs.push([0.5 + 0.5 * a, 0.5 - 0.5 * b]);
        }
        // Two triangles per quad, no vertex shared between faces (each face has its own normal).
        mesh.indices
            .extend([first, first + 1, first + 2, first, first + 2, first + 3]);
    }
    Model {
        name: "cube".into(),
        nodes: vec![Node::new("cube", None)],
        meshes: vec![mesh],
        materials: vec![Material {
            name: "checker".into(),
            base_color: Some(checker()),
            // A neutral, matte stand-in: the game says nothing about metal or gloss here.
            metallic: 0.0,
            roughness: 1.0,
            ..Material::default()
        }],
        skins: vec![],
    }
}

/// An 8 x 8 two-colour checkerboard. `Image` is 8-bit RGBA, rows top to bottom, and a base colour
/// image is sRGB.
fn checker() -> Image {
    let (width, height) = (8u32, 8u32);
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let light = (x + y) % 2 == 0;
            rgba.extend(if light {
                [236, 220, 196, 255]
            } else {
                [196, 70, 52, 255]
            });
        }
    }
    Image {
        width,
        height,
        rgba,
    }
}

fn main() {
    // What a provider crate's own `check-conformance` subcommand does with its arguments.
    std::process::exit(rewrite_bridge::conformance::cli::run(
        &CubeGame,
        std::env::args().skip(1),
    ));
}

#[cfg(test)]
mod tests {
    use rewrite_bridge::conformance::check_provider;

    use super::*;

    #[test]
    fn the_cube_is_valid_and_has_nothing_to_report() {
        let model = CubeGame.load(CUBE).unwrap();
        model.validate().unwrap();
        // 6 faces x 4 corners, 6 faces x 2 triangles
        assert_eq!(model.meshes[0].positions.len(), 24);
        assert_eq!(model.meshes[0].indices.len(), 36);
        assert_eq!(model.bounds(), Some(([-0.5, 0.0, -0.5], [0.5, 1.0, 0.5])));

        let reports = check_provider(&CubeGame, &[] as &[&str], &[]);
        assert_eq!(reports.len(), 1);
        for (id, report) in &reports {
            assert!(report.findings.is_empty(), "{id}: {report}");
        }
    }

    #[test]
    fn list_filters_by_id_or_name_and_load_rejects_unknown_ids() {
        assert_eq!(CubeGame.list("").unwrap().len(), 1);
        assert_eq!(CubeGame.list("CUBE").unwrap().len(), 1);
        assert!(CubeGame.list("tank").unwrap().is_empty());
        assert!(CubeGame.load("props/tank").is_err());
    }

    #[test]
    fn the_check_conformance_subcommand_exits_zero() {
        let mut out = Vec::new();
        let status = rewrite_bridge::conformance::cli::run_to(&CubeGame, [], &mut out);
        assert_eq!(status, 0, "{}", String::from_utf8_lossy(&out));
    }
}
```

Things worth noticing:

- **One node, one mesh, one material.** A `Model` is a node tree (`nodes`, parents before
  children), meshes that hang on nodes (`Mesh::node`) and the materials they use (`Mesh::material`).
- **Attributes are all or nothing.** `normals`, `uvs`, `tangents`, `joints` and `weights` are either
  empty or have one entry per position. The cube has no shared vertices between faces because every
  face has its own normal, the usual way to store a hard edge.
- **Winding.** Triangles are counter-clockwise seen from outside. The `u x v = normal` comment in
  the example is the way to get it right without trial and error.
- **UVs follow glTF.** The origin is the top-left corner of the image and `v` runs down the image,
  the same order as `Image::rgba` (rows top to bottom). A game that puts `v = 0` at the bottom
  needs `1 - v`; nocturne-unpack does exactly that.
- **`Material::default()` is fully metallic** (`metallic: 1.0`, `roughness: 1.0`, as in glTF). If
  you have no data for them, say what you mean: the example asks for matte, `metallic: 0.0`.
- **No tangents.** A model without them is fine; Bevy's plugin generates tangents for materials
  that have a normal map.
- **Call `validate()` before you return.** It is cheap, and the error names the asset and the
  index that is wrong.

What `Model::validate` checks: every node's parent comes before it; every image holds exactly
`width * height * 4` bytes; every skin has one inverse bind matrix per joint and names real
nodes; every mesh has a real material and (unless it is skinned) a real node, optional attributes
that are empty or one per position, a whole number of triangles and indices inside the vertices;
a skinned mesh has joints and weights, joint indices inside the skin, and weights that sum to 1
(within 0.001); a mesh without a skin has no joints. What it cannot see, and the checker
([section 10](#10-test-with-the-checker)) can, is a model that is valid and 100 times too large.

## 6. Space and units

The contract, in one place (the same words as the [README](../README.md#rewrite-bridge)):

- metres;
- +Y up, right-handed, the model's front faces +Z;
- front faces wind counter-clockwise;
- time in seconds, quaternions as x, y, z, w.

Your game almost certainly differs. What the four providers do, from the code:

| provider | the game's space | the conversion |
|---|---|---|
| hytale-unpack | used as it is: a block model's face called `front` is +Z, and nothing is mirrored or rotated | scale only: pixels divided by 64 for characters and attachments, by 32 for props and blocks (the file's `format` field, else the folder), and **1 block = 1 m**. The data never says how long a block is; 1 m fits the player (hitbox 1.85 m, model about 1.89 m) |
| wf-unpack | left-handed, Y up, metres already (Excalibur is 1.84 m) | negate x; swap two corners of every triangle to keep the winding; bone translation x negated; bone rotation `(x, -y, -z, w)`; inverse bind matrices `S * M * S` with `S = diag(-1, 1, 1)`; an instance in a level gets x of the translation negated and the same rotation rule |
| wt-unpack | left-handed, Y up; vehicles face +X with their left side at +Z | `(x, y, z)` becomes `(z, y, x)`: a swap that is also a mirror, so the triangles are rewound |
| nocturne-unpack | Unity's, left-handed | negate x; rotation `(x, -y, -z, w)`; tangent `(-x, y, z, -w)`; triangles reversed; `v` becomes `1 - v`; texture rows flipped to top first; Unity's units are metres |

The mirror rule is the same in three of them and worth copying. nocturne-unpack, `src/asset.rs`:

```rust
/// Unity's left-handed space to glTF's right-handed one: x is negated.
pub mod space {
    pub fn position(p: [f32; 3]) -> [f32; 3] {
        [-p[0], p[1], p[2]]
    }

    /// A rotation: a mirror turns the axis of rotation's y and z around.
    pub fn rotation(q: [f32; 4]) -> [f32; 4] {
        [q[0], -q[1], -q[2], q[3]]
    }

    pub fn tangent(t: [f32; 4]) -> [f32; 4] {
        [-t[0], t[1], t[2], -t[3]]
    }
}
```

A mirror reverses the winding, so mirroring positions and not rewinding the triangles is the classic
way to ship a model that is inside out (the checker's `geometry.winding_flipped`). wf-unpack, `src/scene.rs`:

```rust
if mirror_x {
    // Mirroring flips the winding; swap two corners to keep faces pointing out
    for t in indices.as_chunks_mut::<3>().0 {
        t.swap(1, 2);
    }
}
```

**How to find your game's axes.** Look for something in the data that has an obvious front and an
obvious side. wt-unpack's author found Dagor's vehicles face +X because the Sherman's muzzle node
`emtr_gun_flame` is at x = 3.9 and its `wheel_l_*` nodes are at z = +1.08. Then let the checker
confirm (section 10): `scale.too_small` and `scale.too_large` find a wrong unit,
`orientation.not_upright` a Z-up game read as Y-up, `orientation.feet_off_ground` a wrong origin,
`geometry.winding_flipped` a mirror without a rewind, and `front.not_plus_z` a model whose named
front points (`muzzle`, `eye`, `headlight` ...) point elsewhere. The front is only judged where the
asset names one; otherwise it is reported as unknown and never guessed.

**Write down what you assumed.** Hytale's 1 block = 1 m, and the fact that its block rotations
are verified for yaw only, are in hytale-unpack's README under "What is left out". Anything the
data does not state and you chose is a convention, and a reader who finds a model off by a
factor needs to know it was a choice.

## 7. Materials and textures

`Material` is glTF's metallic-roughness model and `Image` is **8-bit RGBA, rows top to bottom**.
The contract fixes the rest, because `Image` carries no colour-space flag:

| image | contents |
|---|---|
| `base_color`, `emissive` | sRGB |
| `metallic_roughness`, `normal` | linear; roughness in **G**, metalness in **B** |
| `normal` | tangent space, green pointing up (OpenGL), not DirectX |

Factors (`base_color_factor`, `metallic`, `roughness`, `emissive_factor`) are linear and multiply
the images. `emissive_factor` is relative radiance with 1.0 as reference white (the field's docs
have the details).

What the providers do about the usual problems:

- **Compressed textures.** The bridge has no compressed formats, so decode to RGBA8 yourself.
  nocturne-unpack decodes DXT1, DXT5 and BC7 with the `texture2ddecoder` crate; wf-unpack reads
  BC1 to BC7, including BC6H, for which it has a decoder of its own (`src/bc6h.rs`).
- **Normal maps in the other convention.** Both wt-unpack ("DirectX-style green: flip for the OpenGL
  convention") and wf-unpack (green inverted, blue rebuilt from red and green) convert on the way in.
- **Packed channels.** Dagor packs smoothness (red), metalness (blue) and two components of the
  normal (alpha and green) into one texture; wt-unpack splits it into a metallic-roughness image
  (roughness is 255 minus the smoothness) and a normal map whose blue it rebuilds.
- **Shading the game does in a shader.** The bridge has one shading model, so bake the result into
  the images and say so. wf-unpack bakes a layered tint colour into the base colour and the emissive colour
  into the emissive map (the game's `EmissiveMapAtten` becomes `emissive_strength`); wt-unpack bakes camouflage and ambient occlusion into the base
  colour (its camouflage blend is an approximation and the README says so).
- **Neutral stand-ins, labelled.** When the game gives no value, use a plain one and call it one.
  In wt-unpack a part with no metallic-roughness map is matte "painted steel" (`metallic 0`,
  `roughness 0.85`), a wf-unpack material it cannot resolve is `metallic 0`, `roughness 0.6`, and
  hytale-unpack's blocks are `metallic 0`, `roughness 1`. None of these is game data.
- **Alpha.** `AlphaMode::Opaque`, `Mask(cutoff)` (pixels under the cutoff are discarded) or
  `Blend`. hytale-unpack uses `Mask(0.5)` for a texture with transparent texels and `Blend` for a
  face the data marks `transparent`; wt-unpack reads an `atest=N` shader parameter as a cutoff of
  `N / 255` and makes glass `Blend` with an alpha of 0.3. The checker finds the two wrong guesses:
  `material.opaque_transparent_texture` (transparent texels that would show as solid colour) and
  `material.blend_opaque_texture` (blending a texture that is opaque).
- **Double-sided** (`double_sided: true`) is per material, so hytale-unpack makes a material variant for
  double-sided shapes.
- **Budget.** A `Material` owns its images, so a scene whose models share a texture in the game
  still holds a copy per model. wf-unpack's `Warframe::with_max_texture(Some(1024))` halves
  textures with a box filter until they fit (its docs: the clan dojo's 39 models hold 1 GB of images
  at full size), wt-unpack has `SceneOptions::object_texture_px`, and wf-unpack and nocturne-unpack
  have `with_textures(false)` for sweeps that only look at geometry. Offer a limit for anything a
  scene can multiply.

## 8. Skins, animation and sockets

### Skins

A `Skin` is a list of `joints` (indices into `model.nodes`) and one **column-major 4x4 inverse bind
matrix** per joint (`[f32; 16]`). A skinned `Mesh` has `skin: Some(i)`, per-vertex `joints`
(`[u16; 4]`, indices into the skin's joint list) and `weights` (`[f32; 4]`, summing to 1); unused
slots have weight 0. A skinned mesh follows its joints and `Mesh::node` is ignored.

Games store matrices row-major as often as column-major. wf-unpack, `src/bridge.rs`:

```rust
// Row-major in the scene, column-major in the bridge
inverse_bind: sk
    .inverse_binds
    .iter()
    .map(|m| std::array::from_fn(|k| m[k % 4][k / 4]))
    .collect(),
```

If the game stores no inverse bind matrices, derive them from the node pose; wt-unpack's tracks
are skinned to nodes with no axes of their own, so each joint's matrix is a translation by minus
its pivot. The checker finds a collapsed matrix (`skin.inverse_bind_singular`) and a node pose that
is not the bind pose (`skin.bind_pose_mismatch`, information only: some games, Unity's among them,
bind that way).

### Clips

An `AnimatedModel { model, animations, sockets, emissive_strength }` is a `Model` plus extras
beside it, so models built before animation existed still compile. A clip is
`Animation { name, duration_s, channels }`, and each `Channel` moves the translation, rotation or
scale of one node. Its `values` are a flat `f32` list like a glTF accessor: 3 floats per keyframe for
translation and scale, 4 for rotation, and three times as many for `CubicSpline` (in-tangent,
value, out-tangent). `Animation::validate` checks the node indices, strictly increasing times
inside the duration, the value counts and unit quaternions. From the crate's docs:

```rust
robot.animations.push(Animation {
    name: "reach".into(),
    duration_s: 1.0,
    channels: vec![Channel::new(
        1, Target::Translation, Interpolation::Linear,
        vec![0.0, 1.0],                     // seconds
        vec![0.0, 0.0, 0.0, 0.0, 0.5, 0.0], // x, y, z in metres per keyframe
    )],
});
```

Things the four providers had to decide, which you will too:

- **Names.** A clip needs a name that is unique within the model, because consumers look clips up
  by name (`animation.unnamed`, `animation.duplicate_name`, `animation.empty`). hytale-unpack
  names a one-clip animation set after the set (`Walk`), a bigger one `Set/<file name>`, and when a
  set lists one file several times at different speeds appends `@speed` (`Fly/Fly@1.25`) and then
  `#2`, `#3`. wf-unpack uses the clip file's asset path. nocturne-unpack uses the animator's own names
  and lets the checker report the models where the game itself repeats one.
- **Which clips belong to which model.** Games rarely say. Hytale lists a model's `AnimationSets`; for
  a bare model it follows the folder rule of the Blockbench plugin. In Warframe a skeleton fits about
  25,000 clips, so wf-unpack loads none unless asked: `<model path>?animations=FILTER` returns the
  clips whose path matches the filter (a substring, or a glob with `*`, `**` and `?`; `|`
  separates filters) and whose bones are all bones of the model. `wf_unpack::bridge::animated_id` builds
  such an id, and more than `DEFAULT_MAX_CLIPS` (2000) clips is an error that says to narrow the filter.
- **Root motion**, where the game has it, goes in the root node's translation channel (wf-unpack
  does so). The checker reports the speed (`animation.root_motion`, in m/s: a walk is 1 to 2) and
  warns above 100 m/s, which is a unit mistake (`animation.root_motion_extreme`).
- **Things `Animation` has no field for**, such as events (footsteps), play rate and the game's own
  metadata. wf-unpack keeps a side table by clip name (`Warframe::clip_info(name)`, with `events`, root
  motion and `rate`). The cost is that a consumer has to know about it; the alternative would be
  to drop them.
- **Interpolation.** hytale-unpack turns `smooth` keyframes into cubic-spline keyframes whose
  tangents reproduce the Catmull-Rom curve of the Blockbench plugin's preview, and keeps a channel
  that is all linear linear. Quaternion keyframes in neighbouring hemispheres make some players spin
  the long way round (`animation.quaternion_sign_flip`).
- **Units of the clips and of the model must agree.** `animation.pose_extent_mismatch` fires when the
  animated skeleton is more than four times larger or smaller than the rest pose.

### Sockets

`Socket { name, node, offset }` is a named attachment point: an existing node plus an offset from it,
so it follows the node through every animation. It is a list, not a convention such as "nodes named
like this are sockets", because a convention cannot tell a socket from a bone, and an extra node would
shift the indices that skins and clips already use. Of the four, only wt-unpack provides sockets: `turret`,
`gun`, `barrel`, `muzzle`, `emitter`, `wheel_*`, `track_*`, `camera_*`, `hatch_*` and the effect emitters
`emtr_*` of ground vehicles, named after the game's own nodes or, for the turret group, after the role
the unit file gives them. Anything the data does not give is left out (wt-unpack lists the nodes the
unit names and the skeleton lacks), nothing is guessed.

### `load` and `load_animated` agree

The trait asks that `load(id)` equals `load_animated(id)?.model`. The easy way to keep it:

```rust
fn load(&self, id: &str) -> Result<Model, BoxError> {
    Ok(self.load_animated(id)?.model)
}
```

hytale-unpack and nocturne-unpack do exactly this. `check-conformance --compare-load` checks it
(`provider.load_mismatch`, an error); it loads everything twice, so it is off by default.

## 9. Scenes

A level is `AssetKind::Level` in `list` and a `Scene` from `load_scene`. A provider that has none
leaves the default, which returns the `NotSupported` error, and callers can tell "this game has no
scenes" from a failure with `err.downcast_ref::<NotSupported>()`. nocturne-unpack uses it for
demons.

A `Scene { name, models, instances, lights, environment }` is a list of models and a list of places.
`models` are `SceneModel::Inline(Model)` or `SceneModel::Asset(AssetRef)` (a provider's own asset id,
loaded lazily and once); an `Instance` is the index of a model and a `Transform`.

- **Instancing is the point.** A forest is one tree model and ten thousand instances. All four
  providers build one inline model per distinct mesh (or object, or block) and one instance per
  placement: wf-unpack for every model a level places, wt-unpack for each distinct object of a
  level, hytale-unpack for each block name of a prefab, nocturne-unpack for each mesh of a field
  prefab. None of the four uses `SceneModel::Asset` yet.
- **Transforms follow section 6.** wf-unpack negates x of the translation and applies the rotation
  rule to every instance.
- **Lights and sky are optional; do not invent them.** `Light` is directional in lux, point and spot
  in candela (as `KHR_lights_punctual`), and `Environment` is a colour or an equirectangular
  picture. wf-unpack fills them in from the level; nocturne-unpack says it reads no lights.
- **Collision has no field in `Scene`, and each provider that has it does it its own way.**
  wf-unpack returns a level's collision as a scene of its own: `load_scene("<level path>?collision")`
  has the visible scene's instances that have shapes, in the same order and with the same
  transforms, over models that are the PhysX shapes, and a mesh's name says what the shape is
  (`triangles`, `convex`, `box half_extents=[..]`, ...). wt-unpack has
  `WarThunder::load_level_collision(id)` and `load_object_collision(name)`, outside the trait.
  hytale-unpack and nocturne-unpack provide none. The bridge carries no physics data at all yet;
  the testbed labels every physics value as its own assumption.
- **Scenes are heavy.** Everything a scene holds is in memory at once. Offer budgets (section 7),
  report what you left out (wt-unpack returns a `SceneReport`, hytale-unpack a `SceneReport` from
  `load_scene_report`) and give the caller a way to hand memory back: hytale-unpack has
  `Hytale::clear_cache()`, wt-unpack `WarThunder::clear_caches()`, which also asks the allocator to
  return its free pages, because a program that drops a scene otherwise keeps showing the memory as
  resident.

## 10. Test with the checker

`validate` says a model is consistent; the conformance checker says it means the right thing. Wire it
into your command line in a few lines. hytale-unpack, `src/main.rs`:

```rust
/// Check what the provider returns against the rewrite-bridge conventions (scale, facing,
/// winding, skins, clips ...); try `check-conformance --help`
#[command(disable_help_flag = true)]
CheckConformance {
    /// Options of the checker, passed through (`--id`, `--filter`, `--kind`, `--limit`,
    /// `--strict`, `-v`, `--help`)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
},

// and in the match on the subcommand:
Cmd::CheckConformance { args } => {
    // exits with the checker's status: 0, 1 (an asset failed) or 2 (usage)
    std::process::exit(rewrite_bridge::conformance::cli::run(&hy, args))
}
```

`disable_help_flag` and `trailing_var_arg` hand every argument after the subcommand to the checker,
so options of your own, such as `--game`, go before it. Then:

```sh
my-game-tool check-conformance --kind character --limit 20     # a first look
my-game-tool check-conformance --id tiger --id t34 -v          # these assets, with info findings
my-game-tool check-conformance --compare-load                  # also load == load_animated.model
my-game-tool check-conformance --strict                        # warnings fail too
```

The output is one block per asset, then a table of finding codes with counts and example ids; the
exit status is 1 if an asset has an error. A panic in your provider, or a failed load, is a finding on
that asset (`provider.panicked`, `provider.load_failed`), not the end of the run. The module's docs
list every code and why it matters.

What to do with a finding: **fix it, or allow it with a reason.** Some findings are conventions or
the game's own data, and each provider keeps a list in its README of what it sees and why, with the
code. That list is part of the provider: nocturne-unpack's, for instance, explains that
`front.not_plus_z` compares named points such as `h_eye` with the middle of the box and that for some
demons the name is an effect anchor, not an eye. Silence one with `--allow CODE` or
`Expect::default().allowing("code")` and write the reason down. Do not ship a provider with errors.

Check a sample first (`--limit 20`), and check each *kind* of asset: characters, props and levels
fail differently. A sample taken in path order may all be the same sort of thing (twenty attachments
in a row), so pick the examples that exercise what you are about to claim.

**A sweep of your own.** `check-conformance` loads everything the provider lists, which can take
hours for a game with tens of thousands of assets. Also give your command line a `check-bridge`-like
command that loads everything through the provider and validates it, and make it kind to the person
who runs it: `--filter` and `--list` to run a part, progress output with elapsed time and peak
memory while it runs, failures grouped by message with one example each, and a final summary with the
total time. wt-unpack's `check-bridge` prints `i/N (seconds, peak MB)` every 250 vehicles and the
errors by message; hytale-unpack's prints counts per kind, the failures and `peak RSS ... MB, ... s`.
A sweep that runs for a long time in silence looks like a hang and gets killed.

**Tests that need the game must skip without it.** The four providers' tests find the install through
an environment variable (above) and return early when there is none. nocturne-unpack,
`tests/conformance.rs`:

```rust
fn provider() -> Option<Nocturne> {
    let dir = PathBuf::from(std::env::var_os("NOCTURNE_GAME_DIR")?);
    Some(Nocturne::open(&dir).expect("NOCTURNE_GAME_DIR is not a Nocturne HD install"))
}

#[test]
fn demons_have_no_conformance_errors() {
    let Some(p) = provider() else { return };
    // ...
}
```

CI has no game files, so those tests run only on the author's machine. Everything else (your
conversions on a synthetic input, `validate`, the id parsing) must be testable without the game, as
the cube example's tests are.

## 11. Memory and failure rules

Game archives are huge, and a provider is meant to run inside somebody else's game or tool. The
rules the four providers follow, and the reasons:

1. **Never read a whole archive.** Read an index, then seek to one entry. hytale-unpack reads the
   central directory of the 3.5 GB `Assets.zip` and refuses a single entry over 256 MB
   (`READ_LIMIT`; `Assets::with_reader` streams anything bigger); wt-unpack and nocturne-unpack map
   files with `memmap2`; wf-unpack reads one entry of a `.cache` by
   its offset and refuses an implausible length. Do not call `std::fs::read` on a file you did not
   size first.
2. **One asset at a time.** `check_provider` loads, checks and drops each asset before the next, and
   so should every sweep.
3. **Failures are findings, not panics.** Return `Err` for a bad file, with the asset and file in
   the message. The error type is `Box<dyn Error + Send + Sync>`, and a boxed `anyhow` error shows
   only its outermost message, so flatten the chain. nocturne-unpack:

   ```rust
   /// An error with its whole chain of causes in the message (a boxed `anyhow` error shows only the
   /// outermost one).
   fn boxed(e: anyhow::Error) -> BoxError {
       format!("{e:#}").into()
   }
   ```
4. **No global state.** Caches live in the provider, behind a `Mutex`, so that dropping the provider
   or calling a `clear_caches` method gives the memory back (wt-unpack: "nothing is global"). Make
   the provider `Send + Sync`: the testbed holds providers as `Arc<dyn Provider + Send + Sync>` and
   the Bevy plugin's README as `Box<dyn Provider + Send + Sync>`. wf-unpack has a test that fails to
   compile if the provider stops being shareable.
5. **Measure.** Print the peak resident set size at the end of every heavy command, and put the number
   and the time in the README with "about" in front of it, because they drift. hytale-unpack,
   `src/check.rs` (`libc` is a `[target.'cfg(unix)'.dependencies]` entry, because Windows has no
   `getrusage`; nocturne-unpack had to fix its Windows build for exactly this):

   ```rust
   /// Peak resident set size of this process in bytes
   pub fn peak_rss() -> u64 {
       #[cfg(unix)]
       {
           let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
           if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } == 0 {
               let v = ru.ru_maxrss as u64;
               // macOS reports bytes, Linux kilobytes
               return if cfg!(target_os = "macos") {
                   v
               } else {
                   v * 1024
               };
           }
       }
       0
   }
   ```
6. **Say how heavy a command is.** A README line that says a command reads the whole install, takes
   a long time or needs a few gigabytes of memory (measure it, and say "about") saves a user a bad
   afternoon. A command that is slow or large by nature needs `--limit`, `--filter` or a
   one-package form.

## 12. See it

Numbers say a model is plausible; a picture says it is right. [bridge-testbed](https://github.com/patodois/bridge-testbed)
loads items from every provider into one Bevy gallery, under the same light, with a 1 m cube and a
2 m ruler for scale, an arrow on the ground showing the +Z front of every item, and the axes at the
origin. To add yours:

1. add the crate as an optional dependency and a cargo feature in the testbed's `Cargo.toml` (it
   must depend on the crates.io `rewrite-bridge` of the same `0.x` line as every other crate there,
   so that Cargo builds one copy);
2. add the name to `PROVIDER_NAMES` in `src/config.rs` and a match arm in `src/providers.rs` that
   opens your provider from `[providers.<name>] path = "..."` (the arm for the Hytale provider is the
   shortest to copy);
3. put a few `[[items]]` in `testbed.toml` (copy `testbed.example.toml`): `provider`, `id`, `kind`
   (`model`, `animated` or `scene`) and a `label`.

Then, in the testbed:

```sh
cargo run --release -- list --provider <name> --filter <text>   # find ids for the config
cargo run --release -- inspect                  # the numbers of every item as text, no rendering
cargo run --release -- shots --out shots/a      # 512 x 512 shots of every item, headless
cargo run --release -- compare --base shots/a --new shots/b   # per-pixel regression check
cargo run --release                             # the gallery
```

`inspect` and `shots` open no window (`shots` renders off screen, so it also works with the screen
locked). The first `cargo run --release` builds Bevy and takes minutes, so try `inspect` first.
Shots are game content: they stay out of version control.

## 13. Publish

- **Ship no game data.** Not in the repository, not in CI, not in issues. This includes renders,
  glTF exports and screenshots, which are the game's content too. hytale-unpack's README says so of
  its render checks, and the testbed's `.gitignore` covers its shots.
- **Tests and CI work without the game** (section 10). nocturne-unpack's CI runs `fmt`, `clippy` and
  the tests with and without the `bridge` feature on Linux, macOS and Windows.
- **Say what is left out.** Every provider has a section of what its data holds that the bridge
  cannot carry or that the provider does not read: hytale-unpack's random attachment sets,
  `shapeVisible` and biome tints; nocturne-unpack's texture samplers, vertex colours and second UV set; wt-unpack's
  `NOT_APPLIED` list of what its level scenes leave out. It is the most useful part of a README for the next person.
- **Say what you could not verify.** hytale-unpack states which of its block rotations are checked
  against real data (yaw) and which could not be (pitch and roll) and where one line flips the sign.
- **Credit the formats.** hytale-unpack credits the Blockbench plugin that documents its formats;
  nocturne-unpack explains its patched copy of a dependency in `NOTES-upstream.md`.
- **Legal.** Not affiliated, not endorsed, reads files from the player's own install, redistributes
  nothing. The four READMEs have a disclaimer or, for hytale-unpack, a summary of the game's
  published terms and what they do not say. This guide is not legal advice and neither is theirs; read the
  game's terms yourself and leave the decision about reading one's own install to the user.
- **License.** The four providers and this crate are `MIT OR Apache-2.0`.
- **Depend on the crates.io release** (`rewrite-bridge = "0.2"`, section 2), and move every crate you build together to the next `0.x` line in one step, since a different minor line is a different copy.
- **Tell people.** Once the provider is public, add a row to the "Projects using it" table of the
  [README](../README.md#projects-using-it).

## 14. Checklist

- [ ] Library plus thin command line; `bridge` feature; `rewrite-bridge` from crates.io on the same
      `0.x` line as every other crate in the build (one copy in `cargo tree -i rewrite-bridge`).
- [ ] The install path is an argument; a default per OS, an environment variable and `~` expansion
      exist; a wrong folder is an error that names it.
- [ ] `list`: stable ids from the game's paths, only what draws, honest `AssetKind`, sorted, indexed once.
- [ ] `load` calls `validate()`; `load(id)` equals `load_animated(id)?.model`.
- [ ] Metres, +Y up, right-handed, +Z front, counter-clockwise; every assumption written down.
- [ ] Textures are RGBA8 top to bottom; colour sRGB, the rest linear; normal maps green up;
      roughness in G and metalness in B; game shading baked and stated; stand-ins labelled.
- [ ] Clips have unique names; skins have column-major inverse binds; sockets only where the data names them.
- [ ] Levels are scenes with instancing; collision documented; memory budgets offered.
- [ ] `check-conformance` wired; no errors; every remaining finding explained in the README.
- [ ] A sweep command with `--filter`, progress and peak memory; heavy commands say how heavy.
- [ ] No whole-archive reads, no global caches, no panics on bad data.
- [ ] Tests skip without the game; CI runs on the three OSes without game data.
- [ ] Seen in the testbed (`inspect` and `shots`).
- [ ] README: what is left out, what could not be verified, how to get the game files, disclaimer,
      license.
