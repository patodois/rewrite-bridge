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
