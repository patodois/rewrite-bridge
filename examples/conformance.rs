//! The conformance checker on a toy provider, to show what its output looks like and how little
//! a provider crate needs to wire it in.
//!
//! cargo run --example conformance                 # every asset; exits 1 because "wrong" is wrong
//! cargo run --example conformance -- --id right   # one asset
//! cargo run --example conformance -- --help
//!
//! In a real provider crate this is the `check-conformance` subcommand:
//!
//! ```ignore
//! if args.next().as_deref() == Some("check-conformance") {
//!     std::process::exit(rewrite_bridge::conformance::cli::run(&provider, args));
//! }
//! ```

use rewrite_bridge::{AssetInfo, AssetKind, BoxError, Material, Mesh, Model, Node, Provider};

/// Two assets: a tidy 1 m cube, and the same cube in centimetres with flipped winding.
struct Toy;

fn cube(scale: f32, flipped: bool) -> Model {
    // Corners of a cube standing on y = 0, one quad per face with its outward normal.
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
        ..Mesh::default()
    };
    for (n, u, v) in faces {
        let first = mesh.positions.len() as u32;
        for (a, b) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = [0, 1, 2].map(|k| 0.5 * scale * (n[k] + a * u[k] + b * v[k]));
            mesh.positions.push([p[0], p[1] + 0.5 * scale, p[2]]);
            mesh.normals.push(n);
        }
        let quad = [first, first + 1, first + 2, first, first + 2, first + 3];
        mesh.indices.extend(if flipped {
            [quad[0], quad[2], quad[1], quad[3], quad[5], quad[4]]
        } else {
            quad
        });
    }
    Model {
        name: "cube".into(),
        nodes: vec![Node::new("root", None)],
        meshes: vec![mesh],
        materials: vec![Material::default()],
        skins: vec![],
    }
}

impl Provider for Toy {
    fn game(&self) -> &str {
        "Toy Game"
    }

    fn list(&self, filter: &str) -> Result<Vec<AssetInfo>, BoxError> {
        let all = [("right", AssetKind::Prop), ("wrong", AssetKind::Prop)];
        Ok(all
            .iter()
            .filter(|(id, _)| id.contains(filter))
            .map(|&(id, kind)| AssetInfo {
                id: id.into(),
                name: id.into(),
                kind,
            })
            .collect())
    }

    fn load(&self, id: &str) -> Result<Model, BoxError> {
        match id {
            "right" => Ok(cube(1.0, false)),
            // Centimetres read as metres, and the winding flipped on the way.
            "wrong" => Ok(cube(3000.0, true)),
            _ => Err(format!("no asset {id:?}").into()),
        }
    }
}

fn main() {
    std::process::exit(rewrite_bridge::conformance::cli::run(
        &Toy,
        std::env::args().skip(1),
    ));
}
