//! A viewer for a tiny synthetic provider: no game files, everything is generated here.
//!
//! The provider offers a `robot` (rigid and skinned meshes, an emissive visor, two clips, a socket), a
//! `crate` prop and a `yard` scene of instanced trees and crates with a sun, a lamp, a torch and a sky.
//! It shows the whole path from provider to pixels:
//!
//! ```text
//! cargo run --release --example viewer --features viewer -- --screenshot out.png
//! cargo run --release --example viewer --features window     # a window, orbiting the scene
//! ```
//!
//! With `--screenshot FILE` it renders off screen (no window, so it also runs without a display),
//! writes the PNG and exits. Options: `--size 1280x720`, `--clip wave|idle`, `--at SECONDS` (the clip's
//! time to freeze at, default 1.0), `--no-sky`, and `--view EX,EY,EZ,TX,TY,TZ` (camera position and target).

use std::path::PathBuf;
use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::asset::RenderAssetUsages;
use bevy::camera::{Exposure, Hdr, RenderTarget};
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::prelude::*;
use bevy::render::render_resource::TextureFormat;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};
use bevy::time::TimeUpdateStrategy;
use bevy::window::ExitCondition;
use rewrite_bridge::{
    AnimatedModel, Animation, AssetInfo, AssetKind, AssetRef, BoxError, Channel, Environment,
    Instance, Interpolation, Light, LightKind, Model, Node, Provider, Scene, SceneModel, Skin,
    Socket, Target,
};
use rewrite_bridge_bevy::{
    white_luminance, Bridge, BridgeAnimations, BridgeSockets, Providers, RewriteBridgePlugin, Sky,
    SpawnOptions,
};

// ---------------------------------------------------------------------------------------------
// The synthetic provider: models built from boxes and procedural images
// ---------------------------------------------------------------------------------------------

struct Synthetic;

/// Joints and weights of a position, for skinned geometry
type SkinWeights<'a> = &'a dyn Fn(Vec3) -> ([u16; 4], [f32; 4]);

/// Triangles with per-vertex attributes, built from quads
#[derive(Default)]
struct Geo {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    joints: Vec<[u16; 4]>,
    weights: Vec<[f32; 4]>,
    indices: Vec<u32>,
}

impl Geo {
    /// A quad whose corners go counter-clockwise seen from the side `normal` points to
    fn quad(
        &mut self,
        corners: [Vec3; 4],
        normal: Vec3,
        uv_scale: Vec2,
        skin: Option<SkinWeights>,
    ) {
        let base = self.positions.len() as u32;
        for (c, uv) in corners
            .iter()
            .zip([[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]])
        {
            self.positions.push(c.to_array());
            self.normals.push(normal.to_array());
            self.uvs.push([uv[0] * uv_scale.x, uv[1] * uv_scale.y]);
            if let Some(skin) = skin {
                let (j, w) = skin(*c);
                self.joints.push(j);
                self.weights.push(w);
            }
        }
        self.indices
            .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    /// A box; faces have their own vertices, so edges stay sharp. `rings` splits the sides along Y
    /// (for skinning).
    fn cuboid(
        &mut self,
        center: Vec3,
        half: Vec3,
        uv_scale: Vec2,
        rings: u32,
        skin: Option<SkinWeights>,
    ) {
        let axes = [Vec3::X, Vec3::Y, Vec3::Z];
        for axis in 0..3 {
            for sign in [1.0, -1.0] {
                let n = axes[axis] * sign;
                let (mut u, mut v) = (axes[(axis + 1) % 3], axes[(axis + 2) % 3]);
                if sign < 0.0 {
                    std::mem::swap(&mut u, &mut v);
                }
                let (hu, hv) = (half.dot(u), half.dot(v));
                let c = center + n * half.dot(axes[axis]);
                // Split along Y where that is one of the face's own directions
                let split_v = rings > 1 && v == Vec3::Y;
                let split_u = rings > 1 && u == Vec3::Y;
                let (nu, nv) = (
                    if split_u { rings } else { 1 },
                    if split_v { rings } else { 1 },
                );
                for iu in 0..nu {
                    for iv in 0..nv {
                        let (u0, u1) = (
                            -1.0 + 2.0 * iu as f32 / nu as f32,
                            -1.0 + 2.0 * (iu + 1) as f32 / nu as f32,
                        );
                        let (v0, v1) = (
                            -1.0 + 2.0 * iv as f32 / nv as f32,
                            -1.0 + 2.0 * (iv + 1) as f32 / nv as f32,
                        );
                        let p = |a: f32, b: f32| c + u * hu * a + v * hv * b;
                        self.quad(
                            [p(u0, v0), p(u1, v0), p(u1, v1), p(u0, v1)],
                            n,
                            uv_scale,
                            skin,
                        );
                    }
                }
            }
        }
    }

    fn translated(mut self, by: Vec3) -> Self {
        for p in &mut self.positions {
            *p = (Vec3::from(*p) + by).to_array();
        }
        self
    }

    fn into_mesh(
        self,
        name: &str,
        node: usize,
        material: usize,
        skin: Option<usize>,
    ) -> rewrite_bridge::Mesh {
        rewrite_bridge::Mesh {
            name: name.into(),
            node,
            material,
            positions: self.positions,
            normals: self.normals,
            uvs: self.uvs,
            joints: self.joints,
            weights: self.weights,
            skin,
            indices: self.indices,
            ..Default::default()
        }
    }
}

fn image(w: u32, h: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> rewrite_bridge::Image {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            rgba.extend(pixel(x, y));
        }
    }
    rewrite_bridge::Image {
        width: w,
        height: h,
        rgba,
    }
}

/// Two-tone checkerboard of `cells` x `cells`, sRGB
fn checker(size: u32, cells: u32, a: [u8; 3], b: [u8; 3]) -> rewrite_bridge::Image {
    image(size, size, |x, y| {
        let c = if ((x * cells / size) + (y * cells / size)).is_multiple_of(2) {
            a
        } else {
            b
        };
        [c[0], c[1], c[2], 255]
    })
}

/// A tangent-space normal map of domed tiles, OpenGL style (green up the texture)
fn domes(size: u32, tiles: u32) -> rewrite_bridge::Image {
    image(size, size, |x, y| {
        let t = size / tiles;
        let (dx, dy) = (
            (x % t) as f32 / t as f32 * 2.0 - 1.0,
            (y % t) as f32 / t as f32 * 2.0 - 1.0,
        );
        let n = Vec3::new(dx * 0.9, -dy * 0.9, 1.0).normalize();
        let e = |v: f32| ((v * 0.5 + 0.5) * 255.0).round() as u8;
        [e(n.x), e(n.y), e(n.z), 255]
    })
}

fn material(name: &str, base: [f32; 4], metallic: f32, roughness: f32) -> rewrite_bridge::Material {
    rewrite_bridge::Material {
        name: name.into(),
        base_color_factor: base,
        metallic,
        roughness,
        ..Default::default()
    }
}

/// Rest position of each node of the robot in model space
const ROBOT_NODES: [(&str, Option<usize>, [f32; 3]); 6] = [
    ("root", None, [0.0, 0.0, 0.0]),
    ("torso", Some(0), [0.0, 1.0, 0.0]),
    ("head", Some(1), [0.0, 0.75, 0.0]),
    ("shoulder_r", Some(1), [0.5, 0.6, 0.0]),
    ("elbow_r", Some(3), [0.0, -0.45, 0.0]),
    ("hand_r", Some(4), [0.0, -0.45, 0.0]),
];

fn robot() -> AnimatedModel {
    let nodes: Vec<Node> = ROBOT_NODES
        .iter()
        .map(|&(n, p, t)| Node {
            translation: t,
            ..Node::new(n, p)
        })
        .collect();
    let globals = rewrite_bridge_bevy::rest_globals(&Model {
        nodes: nodes.clone(),
        ..Default::default()
    });
    let at = |node: usize| globals[node].w_axis.truncate();

    let plate = material("plate", [1.0; 4], 0.7, 0.35);
    let mut plate = plate;
    plate.base_color = Some(checker(64, 8, [150, 170, 200], [110, 130, 165]));
    plate.normal = Some(domes(64, 8));
    let mut visor = material("visor", [0.0, 0.0, 0.0, 1.0], 0.0, 0.2);
    // Strength 2 (below) doubles it: cyan, 0.2 / 0.9 / 1.0 of reference white
    visor.emissive_factor = [0.1, 0.45, 0.5];
    let arm = material("arm", [1.0, 0.45, 0.1, 1.0], 0.1, 0.5);

    let mut meshes = Vec::new();
    // Rigid parts hang on their own nodes; their positions are in the node's space
    let rigid = |center: Vec3, half: Vec3, node: usize| {
        let mut g = Geo::default();
        g.cuboid(center, half, Vec2::ONE, 1, None);
        g.translated(-at(node))
    };
    meshes.push(
        rigid(Vec3::new(0.0, 1.0, 0.0), Vec3::new(0.4, 0.55, 0.25), 1)
            .into_mesh("torso", 1, 0, None),
    );
    meshes.push(
        rigid(Vec3::new(0.0, 1.75, 0.0), Vec3::new(0.28, 0.25, 0.25), 2)
            .into_mesh("head", 2, 0, None),
    );
    meshes.push(
        rigid(Vec3::new(0.0, 1.78, 0.2), Vec3::new(0.2, 0.07, 0.07), 2)
            .into_mesh("visor", 2, 1, None),
    );
    meshes.push(
        rigid(Vec3::new(-0.65, 1.15, 0.0), Vec3::new(0.12, 0.45, 0.12), 1)
            .into_mesh("arm_l", 1, 0, None),
    );
    meshes.push(
        rigid(Vec3::new(-0.2, 0.25, 0.0), Vec3::new(0.14, 0.25, 0.16), 0)
            .into_mesh("leg_l", 0, 0, None),
    );
    meshes.push(
        rigid(Vec3::new(0.2, 0.25, 0.0), Vec3::new(0.14, 0.25, 0.16), 0)
            .into_mesh("leg_r", 0, 0, None),
    );

    // The right arm is one skinned column from shoulder to hand: the shoulder moves the top, the elbow the
    // bottom, and the vertices around the elbow take both
    let elbow_y = at(4).y;
    let weight = move |p: Vec3| {
        let t = ((elbow_y + 0.18 - p.y) / 0.36).clamp(0.0, 1.0);
        let lower = t * t * (3.0 - 2.0 * t);
        ([0u16, 1, 0, 0], [1.0 - lower, lower, 0.0, 0.0])
    };
    let mut arm_geo = Geo::default();
    arm_geo.cuboid(
        Vec3::new(0.5, (at(3).y + at(5).y) / 2.0 - 0.05, 0.0),
        Vec3::new(0.1, 0.5, 0.1),
        Vec2::ONE,
        8,
        Some(&weight),
    );
    meshes.push(arm_geo.into_mesh("arm_r", 0, 2, Some(0)));

    let skin = Skin {
        joints: vec![3, 4],
        inverse_bind: [3, 4]
            .iter()
            .map(|&j| globals[j].inverse().to_cols_array())
            .collect(),
    };

    let mut robot = AnimatedModel::new(Model {
        name: "robot".into(),
        nodes,
        meshes,
        materials: vec![plate, visor, arm],
        skins: vec![skin],
    });
    robot.emissive_strength = vec![1.0, 2.0, 1.0];

    let q = |q: Quat| q.to_array();
    let z = |deg: f32| Quat::from_rotation_z(deg.to_radians());
    let flat = |v: Vec<Vec<f32>>| v.concat();
    robot.animations.push(Animation {
        name: "wave".into(),
        duration_s: 2.0,
        channels: vec![
            // The arm swings up over the head and back
            Channel::new(
                3,
                Target::Rotation,
                Interpolation::Linear,
                vec![0.0, 0.5, 1.5, 2.0],
                flat(vec![
                    q(z(0.0)).to_vec(),
                    q(z(150.0)).to_vec(),
                    q(z(150.0)).to_vec(),
                    q(z(0.0)).to_vec(),
                ]),
            ),
            // The forearm waves
            Channel::new(
                4,
                Target::Rotation,
                Interpolation::CubicSpline,
                vec![0.0, 0.5, 1.0, 1.5, 2.0],
                {
                    let zero = [0.0f32; 4];
                    let keys = [z(0.0), z(35.0), z(-35.0), z(35.0), z(0.0)];
                    keys.iter()
                        .flat_map(|k| [zero, q(*k), zero])
                        .flatten()
                        .collect()
                },
            ),
            // The torso bobs, the head nods in steps
            Channel::new(
                1,
                Target::Translation,
                Interpolation::Linear,
                vec![0.0, 1.0, 2.0],
                vec![0.0, 1.0, 0.0, 0.0, 1.06, 0.0, 0.0, 1.0, 0.0],
            ),
            Channel::new(
                2,
                Target::Rotation,
                Interpolation::Step,
                vec![0.0, 0.5, 1.5],
                flat(vec![
                    q(Quat::IDENTITY).to_vec(),
                    q(Quat::from_rotation_y(0.4)).to_vec(),
                    q(Quat::IDENTITY).to_vec(),
                ]),
            ),
        ],
    });
    robot.animations.push(Animation {
        name: "idle".into(),
        duration_s: 3.0,
        channels: vec![Channel::new(
            2,
            Target::Rotation,
            Interpolation::Linear,
            vec![0.0, 1.5, 3.0],
            flat(vec![
                q(Quat::from_rotation_y(-0.3)).to_vec(),
                q(Quat::from_rotation_y(0.3)).to_vec(),
                q(Quat::from_rotation_y(-0.3)).to_vec(),
            ]),
        )],
    });
    // Where a tool goes: at the palm
    robot.sockets.push(Socket::new(
        "grip",
        5,
        rewrite_bridge::Transform::from_translation([0.0, -0.08, 0.0]),
    ));
    robot
}

fn crate_model() -> Model {
    let mut wood = material("wood", [1.0; 4], 0.0, 0.8);
    wood.base_color = Some(checker(32, 4, [150, 100, 55], [120, 80, 40]));
    let mut g = Geo::default();
    g.cuboid(
        Vec3::new(0.0, 0.4, 0.0),
        Vec3::splat(0.4),
        Vec2::ONE,
        1,
        None,
    );
    Model {
        name: "crate".into(),
        nodes: vec![Node::new("root", None)],
        meshes: vec![g.into_mesh("crate", 0, 0, None)],
        materials: vec![wood],
        skins: vec![],
    }
}

fn tree_model() -> Model {
    let bark = material("bark", [0.35, 0.22, 0.12, 1.0], 0.0, 0.9);
    let leaves = material("leaves", [0.15, 0.4, 0.12, 1.0], 0.0, 0.7);
    let mut trunk = Geo::default();
    trunk.cuboid(
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(0.18, 1.0, 0.18),
        Vec2::ONE,
        1,
        None,
    );
    // The crown hangs on its own node, so every tree has a mesh that is not at the model's origin
    let mut crown = Geo::default();
    crown.cuboid(Vec3::ZERO, Vec3::new(0.9, 0.9, 0.9), Vec2::ONE, 1, None);
    let mut crown_node = Node::new("crown", Some(0));
    crown_node.translation = [0.0, 2.4, 0.0];
    crown_node.rotation = Quat::from_rotation_y(0.4).to_array();
    Model {
        name: "tree".into(),
        nodes: vec![Node::new("root", None), crown_node],
        meshes: vec![
            trunk.into_mesh("trunk", 0, 0, None),
            crown.into_mesh("crown", 1, 1, None),
        ],
        materials: vec![bark, leaves],
        skins: vec![],
    }
}

fn ground_model() -> Model {
    let mut grass = material("grass", [1.0; 4], 0.0, 1.0);
    grass.base_color = Some(checker(64, 16, [70, 110, 55], [60, 98, 48]));
    let mut g = Geo::default();
    let h = 30.0;
    g.quad(
        [
            Vec3::new(-h, 0.0, h),
            Vec3::new(h, 0.0, h),
            Vec3::new(h, 0.0, -h),
            Vec3::new(-h, 0.0, -h),
        ],
        Vec3::Y,
        Vec2::splat(30.0),
        None,
    );
    Model {
        name: "ground".into(),
        nodes: vec![Node::new("root", None)],
        meshes: vec![g.into_mesh("ground", 0, 0, None)],
        materials: vec![grass],
        skins: vec![],
    }
}

/// The direction the sun's light travels
const SUN_RAYS: Vec3 = Vec3::new(-0.5, -0.6, -0.55);

/// An equirectangular sky, laid out as the core crate documents, with the sun disc where the sun's light
/// comes from
fn sky_picture() -> rewrite_bridge::Image {
    let sun = -SUN_RAYS.normalize();
    image(512, 256, |x, y| {
        let theta = ((x as f32 + 0.5) / 512.0 - 0.5) * std::f32::consts::TAU;
        let phi = (0.5 - (y as f32 + 0.5) / 256.0) * std::f32::consts::PI;
        let d = Vec3::new(theta.sin() * phi.cos(), phi.sin(), -theta.cos() * phi.cos());
        let sky = Vec3::new(0.75, 0.85, 0.95)
            .lerp(Vec3::new(0.2, 0.42, 0.85), d.y.clamp(0.0, 1.0).sqrt());
        let ground = Vec3::new(0.3, 0.33, 0.28);
        let mut c = if d.y >= 0.0 { sky } else { ground };
        let disc = ((d.dot(sun) - 0.9985) / 0.0012).clamp(0.0, 1.0);
        c = c.lerp(Vec3::new(1.0, 0.96, 0.8), disc);
        [
            (c.x * 255.0) as u8,
            (c.y * 255.0) as u8,
            (c.z * 255.0) as u8,
            255,
        ]
    })
}

fn transform(t: Transform) -> rewrite_bridge::Transform {
    rewrite_bridge::Transform {
        translation: t.translation.to_array(),
        rotation: t.rotation.to_array(),
        scale: t.scale.to_array(),
    }
}

fn yard() -> Scene {
    let mut instances = vec![Instance::new(0, rewrite_bridge::Transform::IDENTITY)];
    // A ring of trees, every third one mirrored, all sharing one tree mesh
    for i in 0..14 {
        let a = i as f32 / 14.0 * std::f32::consts::TAU;
        let r = 8.0 + (i % 3) as f32 * 1.5;
        let s = 0.8 + 0.06 * (i % 5) as f32;
        let mirror = if i % 3 == 0 { -s } else { s };
        instances.push(Instance::new(
            1,
            transform(Transform {
                translation: Vec3::new(a.cos() * r, 0.0, a.sin() * r),
                rotation: Quat::from_rotation_y(a * 3.0),
                scale: Vec3::new(mirror, s, s),
            }),
        ));
    }
    // Crates, stacked and scattered, some mirrored
    for (x, z, turn, mirror) in [
        (-2.0, 1.5, 0.3, 1.0),
        (-2.9, 2.2, 1.1, -1.0),
        (3.2, -1.0, 0.7, 1.0),
        (-1.2, -2.5, 2.0, 1.0),
        (4.5, 2.5, 0.1, -1.0),
    ] {
        instances.push(Instance::new(
            2,
            transform(Transform {
                translation: Vec3::new(x, 0.0, z),
                rotation: Quat::from_rotation_y(turn),
                scale: Vec3::new(mirror, 1.0, 1.0),
            }),
        ));
    }
    let aim =
        |from: Vec3, to: Vec3| transform(Transform::from_translation(from).looking_at(to, Vec3::Y));
    Scene {
        name: "yard".into(),
        models: vec![
            SceneModel::Inline(ground_model()),
            SceneModel::Inline(tree_model()),
            SceneModel::Asset(AssetRef::local("crate")),
        ],
        instances,
        lights: vec![
            Light {
                name: "sun".into(),
                kind: LightKind::Directional,
                color: [1.0, 0.96, 0.88],
                intensity: 12_000.0,
                transform: transform(Transform::IDENTITY.looking_to(SUN_RAYS, Vec3::Y)),
            },
            Light {
                name: "lamp".into(),
                kind: LightKind::Point { range: Some(8.0) },
                color: [1.0, 0.6, 0.3],
                intensity: 3_000.0,
                transform: rewrite_bridge::Transform::from_translation([-2.4, 1.6, 2.4]),
            },
            Light {
                name: "torch".into(),
                kind: LightKind::Spot {
                    range: None,
                    inner_cone_angle: 0.15,
                    outer_cone_angle: 0.4,
                },
                color: [0.6, 0.8, 1.0],
                intensity: 40_000.0,
                transform: aim(Vec3::new(3.0, 4.5, 4.0), Vec3::new(0.0, 1.6, 0.0)),
            },
        ],
        environment: Some(Environment::Equirectangular {
            image: sky_picture(),
            intensity: 1.0,
        }),
    }
}

impl Provider for Synthetic {
    fn game(&self) -> &str {
        "Synthetic"
    }

    fn list(&self, filter: &str) -> Result<Vec<AssetInfo>, BoxError> {
        let all = [
            ("robot", AssetKind::Character),
            ("crate", AssetKind::Prop),
            ("yard", AssetKind::Level),
        ];
        Ok(all
            .into_iter()
            .filter(|(id, _)| id.contains(filter))
            .map(|(id, kind)| AssetInfo {
                id: id.into(),
                name: id.into(),
                kind,
            })
            .collect())
    }

    fn load(&self, id: &str) -> Result<Model, BoxError> {
        match id {
            "robot" => Ok(robot().model),
            "crate" => Ok(crate_model()),
            other => Err(format!("no asset {other}").into()),
        }
    }

    fn load_animated(&self, id: &str) -> Result<AnimatedModel, BoxError> {
        if id == "robot" {
            Ok(robot())
        } else {
            self.load(id).map(AnimatedModel::from)
        }
    }

    fn load_scene(&self, id: &str) -> Result<Scene, BoxError> {
        match id {
            "yard" => Ok(yard()),
            other => Err(format!("no scene {other}").into()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The viewer: a few lines to get the scene in, the rest is camera and screenshot plumbing
// ---------------------------------------------------------------------------------------------

#[derive(Resource, Clone)]
struct Args {
    screenshot: Option<PathBuf>,
    size: (u32, u32),
    clip: String,
    at: f32,
    sky: bool,
    /// Camera position and what it looks at
    view: Option<(Vec3, Vec3)>,
}

fn parse_args() -> Args {
    let mut args = Args {
        screenshot: None,
        size: (1280, 720),
        clip: "wave".into(),
        at: 1.0,
        sky: true,
        view: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--screenshot" => args.screenshot = it.next().map(PathBuf::from),
            "--size" => {
                if let Some((w, h)) = it.next().as_deref().and_then(|s| s.split_once('x')) {
                    args.size = (w.parse().expect("width"), h.parse().expect("height"));
                }
            }
            "--clip" => args.clip = it.next().expect("a clip name"),
            "--at" => args.at = it.next().and_then(|s| s.parse().ok()).expect("seconds"),
            "--no-sky" => args.sky = false,
            // --view EX,EY,EZ,TX,TY,TZ
            "--view" => {
                let v: Vec<f32> = it
                    .next()
                    .expect("six numbers")
                    .split(',')
                    .map(|n| n.parse().expect("a number"))
                    .collect();
                args.view = Some((Vec3::new(v[0], v[1], v[2]), Vec3::new(v[3], v[4], v[5])));
            }
            other => panic!("unknown argument {other}"),
        }
    }
    args
}

#[derive(Resource)]
struct Provide(Synthetic);

#[derive(Resource)]
struct ScreenshotTarget(Handle<Image>);

#[derive(Component)]
struct Orbit;

fn main() {
    let args = parse_args();
    let mut app = App::new();
    // The camera's exposure: the lights (lux, candela) are balanced against it, and so is the sky. Emissive
    // colours are not exposed by default, so a factor of 1.0 is white without any scale.
    let exposure = Exposure { ev100: 12.0 };
    let options = SpawnOptions {
        environment_scale: white_luminance(exposure),
        light_shadows: true,
        ..Default::default()
    };
    app.insert_resource(args.clone())
        .insert_resource(Provide(Synthetic))
        .insert_resource(CameraExposure(exposure));
    if args.screenshot.is_some() {
        // Off screen: no window, and a loop that runs until the picture is taken
        let window = WindowPlugin {
            primary_window: None,
            exit_condition: ExitCondition::DontExit,
            ..default()
        };
        app.add_plugins((
            DefaultPlugins.set(window).set(bevy::render::RenderPlugin {
                synchronous_pipeline_compilation: true,
                ..default()
            }),
            ScheduleRunnerPlugin::run_loop(Duration::ZERO),
        ))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    } else {
        #[cfg(feature = "window")]
        app.add_plugins(DefaultPlugins);
        #[cfg(not(feature = "window"))]
        panic!("without --screenshot this needs a window: build with --features window");
    }
    app.add_plugins(RewriteBridgePlugin::with_options(options))
        .add_systems(Startup, setup)
        .add_systems(Update, (play_when_ready, take_screenshot, orbit))
        .run();
}

#[derive(Resource)]
struct CameraExposure(Exposure);

/// The whole bridge side: load the provider's assets and spawn them
fn setup(
    mut commands: Commands,
    mut bridge: Bridge,
    provider: Res<Provide>,
    args: Res<Args>,
    exposure: Res<CameraExposure>,
) {
    // The scene's models come from the same provider (the crate is an asset reference)
    let mut scene = provider.0.load_scene("yard").unwrap();
    if !args.sky {
        scene.environment = None;
    }
    let scene = bridge
        .spawn_scene(scene, &Providers::new(&provider.0))
        .expect("a valid scene");

    let robot = bridge.spawn_animated(provider.0.load_animated("robot").unwrap());
    // A tool in the robot's hand, parented to the socket: it follows the arm through every clip
    let grip = robot.sockets["grip"];
    let (mesh, material) = (
        bridge.assets.meshes.add(Cuboid::new(0.06, 0.06, 0.6)),
        bridge.assets.materials.add(StandardMaterial {
            base_color: Color::srgb(0.9, 0.9, 0.2),
            emissive: LinearRgba::rgb(1.0, 0.9, 0.1),
            ..default()
        }),
    );
    bridge.commands.spawn((
        Mesh3d(mesh),
        MeshMaterial3d(material),
        Transform::from_xyz(0.0, 0.0, 0.3),
        ChildOf(grip),
    ));
    bridge
        .commands
        .entity(robot.root)
        .insert(Transform::from_xyz(0.0, 0.0, 0.0));

    let target = args.screenshot.is_some().then(|| {
        let mut image = Image::new_target_texture(
            args.size.0,
            args.size.1,
            TextureFormat::Rgba8UnormSrgb,
            None,
        );
        image.asset_usage = RenderAssetUsages::RENDER_WORLD;
        bridge.assets.images.add(image)
    });
    let mut camera = commands.spawn((
        Camera3d::default(),
        Hdr,
        Tonemapping::AcesFitted,
        exposure.0,
        {
            let (eye, target) = args
                .view
                .unwrap_or((Vec3::new(3.2, 2.0, 5.2), Vec3::new(-0.3, 1.1, 0.0)));
            Transform::from_translation(eye).looking_at(target, Vec3::Y)
        },
        Orbit,
    ));
    if let Some(target) = &target {
        camera.insert(RenderTarget::from(target.clone()));
    }
    let camera = camera.id();
    if let Some(target) = target {
        commands.insert_resource(ScreenshotTarget(target));
    }
    // The scene's sky goes on the camera; without one, a flat colour does the same job
    let sky = scene.sky.unwrap_or(Sky::Color {
        clear: Color::linear_rgb(0.5, 0.65, 0.85),
        ambient: Color::WHITE,
        ambient_brightness: 300.0,
    });
    sky.apply(&mut commands, camera);
}

/// Starts the chosen clip on the robot once its player exists
fn play_when_ready(
    mut players: Query<(&BridgeAnimations, &mut AnimationPlayer), Added<BridgeAnimations>>,
    args: Res<Args>,
) {
    for (clips, mut player) in &mut players {
        match clips.play(&mut player, &args.clip) {
            Some(playing) => {
                playing.repeat();
            }
            None => error!(
                "no clip {:?}; the robot has {:?}",
                args.clip,
                clips.names().collect::<Vec<_>>()
            ),
        }
    }
}

/// Renders for a while so shadows and the sky's lighting settle, freezes the clip at `--at`, takes the
/// picture and exits once it is written
fn take_screenshot(
    mut commands: Commands,
    mut frame: Local<u32>,
    args: Res<Args>,
    target: Option<Res<ScreenshotTarget>>,
    mut players: Query<(&BridgeAnimations, &mut AnimationPlayer)>,
    sockets: Query<&BridgeSockets>,
    mut exit: MessageWriter<AppExit>,
) {
    let Some(target) = target else { return };
    *frame += 1;
    match *frame {
        90 => {
            for (clips, mut player) in &mut players {
                if let Some(clip) = clips.get(&args.clip) {
                    if let Some(playing) = player.animation_mut(clip.node) {
                        playing.seek_to(args.at).pause();
                    }
                }
            }
            info!(
                "sockets: {:?}",
                sockets.iter().flat_map(|s| s.0.keys()).collect::<Vec<_>>()
            );
        }
        100 => {
            let path = args.screenshot.clone().expect("a path");
            commands
                .spawn(Screenshot::image(target.0.clone()))
                .observe(save_to_disk(path));
        }
        // Give the read-back and the write a few frames
        130 => {
            exit.write(AppExit::Success);
        }
        _ => {}
    }
}

/// In a window the camera circles the scene
fn orbit(time: Res<Time>, args: Res<Args>, mut cameras: Query<&mut Transform, With<Orbit>>) {
    if args.screenshot.is_some() {
        return;
    }
    for mut t in &mut cameras {
        let a = time.elapsed_secs() * 0.25;
        *t = Transform::from_xyz(a.sin() * 6.5, 2.2, a.cos() * 6.5)
            .looking_at(Vec3::new(0.0, 1.1, 0.0), Vec3::Y);
    }
}
