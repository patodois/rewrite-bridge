//! Common model types for game rewrites, so a scene can mix assets read from different games.
//!
//! Each game's reader implements [`Provider`] and converts its own data into a [`Model`]. Every
//! model follows glTF 2.0's conventions, so a renderer never needs to know which game it came from:
//!
//! - units are metres;
//! - +Y is up, the space is right-handed and a model's front faces +Z;
//! - front faces wind counter-clockwise;
//! - colour and emissive images are sRGB, the others are linear;
//! - metallic-roughness images keep roughness in G and metalness in B;
//! - normal maps are tangent-space with +Y (green) pointing up the texture, OpenGL style.
//!
//! Nothing here reads game files, and no game data belongs in this crate.
//!
//! Beyond static models there are, all additive and all in the same conventions (seconds for time,
//! quaternions as x, y, z, w):
//!
//! - [`Animation`] clips, kept next to a model in an [`AnimatedModel`] together with its
//!   [`Socket`]s (named attachment points) and optional per-material emissive strengths;
//! - a [`Scene`]: models (inline, or by asset id to load lazily) placed many times as
//!   [`Instance`]s, with optional [`Light`]s and an [`Environment`].
//!
//! A provider returns them from [`Provider::load_animated`] and [`Provider::load_scene`], which
//! have defaults so providers written before they existed keep compiling and working:
//!
//! ```
//! use rewrite_bridge::{
//!     Animation, AnimatedModel, AssetInfo, AssetKind, AssetRef, BoxError, Channel, Instance,
//!     Interpolation, Model, Node, Provider, Scene, SceneModel, Socket, Target, Transform,
//! };
//!
//! struct MyGame;
//!
//! impl Provider for MyGame {
//!     fn game(&self) -> &str { "My Game" }
//!     fn list(&self, _filter: &str) -> Result<Vec<AssetInfo>, BoxError> {
//!         Ok(vec![AssetInfo { id: "robot".into(), name: "Robot".into(), kind: AssetKind::Character }])
//!     }
//!     // The plain model keeps working for consumers that know nothing about clips.
//!     fn load(&self, id: &str) -> Result<Model, BoxError> {
//!         Ok(self.load_animated(id)?.model)
//!     }
//!     fn load_animated(&self, _id: &str) -> Result<AnimatedModel, BoxError> {
//!         let mut robot = AnimatedModel::new(Model {
//!             name: "robot".into(),
//!             nodes: vec![Node::new("root", None), Node::new("hand", Some(0))],
//!             ..Model::default()
//!         });
//!         robot.animations.push(Animation {
//!             name: "reach".into(),
//!             duration_s: 1.0,
//!             channels: vec![Channel::new(
//!                 1, Target::Translation, Interpolation::Linear,
//!                 vec![0.0, 1.0],                   // seconds
//!                 vec![0.0, 0.0, 0.0, 0.0, 0.5, 0.0], // x, y, z in metres per keyframe
//!             )],
//!         });
//!         robot.sockets.push(Socket::new("grip", 1, Transform::IDENTITY));
//!         robot.validate()?;
//!         Ok(robot)
//!     }
//!     fn load_scene(&self, _id: &str) -> Result<Scene, BoxError> {
//!         let scene = Scene {
//!             name: "yard".into(),
//!             models: vec![SceneModel::Asset(AssetRef::local("robot"))],
//!             // Two robots, one model: loaded once, drawn twice.
//!             instances: vec![
//!                 Instance::new(0, Transform::from_translation([0.0, 0.0, 0.0])),
//!                 Instance::new(0, Transform::from_translation([3.0, 0.0, 0.0])),
//!             ],
//!             ..Scene::default()
//!         };
//!         scene.validate()?;
//!         Ok(scene)
//!     }
//! }
//!
//! let game = MyGame;
//! assert_eq!(game.load_animated("robot").unwrap().animations.len(), 1);
//! assert_eq!(game.load_scene("yard").unwrap().instances.len(), 2);
//! ```
//!
//! With the `link` feature, the `link` module adds a passthrough channel: two processes on
//! 127.0.0.1 exchange camera, poses, collision and frames as described in `docs/CONTRACT.md`.

use std::error::Error;
use std::fmt;

pub mod animated;
pub mod animation;
pub mod scene;
pub mod transform;

#[cfg(feature = "link")]
pub mod link;

pub use animated::{AnimatedModel, Socket};
pub use animation::{Animation, Channel, Interpolation, Target};
pub use scene::{AssetRef, Environment, Instance, Light, LightKind, Scene, SceneModel};
pub use transform::{Transform, QUATERNION_TOLERANCE};

/// Errors from providers, boxed so each game reader can keep its own error type.
pub type BoxError = Box<dyn Error + Send + Sync>;

/// A game reader that can list and load assets from the player's own installation.
pub trait Provider {
    /// Short name of the game, e.g. "War Thunder".
    fn game(&self) -> &str;

    /// Assets whose id or name contains `filter` (all of them for an empty filter).
    fn list(&self, filter: &str) -> Result<Vec<AssetInfo>, BoxError>;

    /// Loads one asset by the id [`Provider::list`] gave for it.
    fn load(&self, id: &str) -> Result<Model, BoxError>;

    /// Loads one asset with its animation clips, attachment points and emissive strengths.
    ///
    /// The default wraps [`Provider::load`] with none of those, so existing providers need not
    /// change. A provider that overrides this should keep `load(id)` equal to
    /// `load_animated(id)?.model`, so consumers that only know about models see the same asset.
    fn load_animated(&self, id: &str) -> Result<AnimatedModel, BoxError> {
        self.load(id).map(AnimatedModel::from)
    }

    /// Loads a scene (a level) by id. Ids of levels are the ones [`Provider::list`] gives, usually
    /// with [`AssetKind::Level`]; the provider decides which of its assets are scenes.
    ///
    /// The default returns [`NotSupported`], which callers can find with
    /// `err.downcast_ref::<NotSupported>()` to tell "this game has no scenes" from a real failure.
    fn load_scene(&self, id: &str) -> Result<Scene, BoxError> {
        let _ = id;
        Err(Box::new(NotSupported {
            game: self.game().to_string(),
            what: "scenes".to_string(),
        }))
    }
}

/// What the default [`Provider::load_scene`] returns: the provider does not offer this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotSupported {
    /// [`Provider::game`] of the provider that said no.
    pub game: String,
    /// What was asked for, e.g. "scenes".
    pub what: String,
}

impl fmt::Display for NotSupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} not supported", self.game, self.what)
    }
}

impl Error for NotSupported {}

/// One entry of a provider's listing.
#[derive(Clone, Debug, PartialEq)]
pub struct AssetInfo {
    /// Stable id to pass to [`Provider::load`]; its format is up to the provider.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    pub kind: AssetKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetKind {
    Vehicle,
    Character,
    Weapon,
    Prop,
    Level,
    Other,
}

/// A model: a node hierarchy with meshes hung on it, their materials, and optional skins.
///
/// Animation clips, attachment points and emissive strengths are not here but in
/// [`AnimatedModel`], next to the model, so this struct can stay as it is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Model {
    pub name: String,
    /// Parents always come before their children.
    pub nodes: Vec<Node>,
    pub meshes: Vec<Mesh>,
    pub materials: Vec<Material>,
    pub skins: Vec<Skin>,
}

/// A transform in the hierarchy, relative to its parent.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub name: String,
    pub parent: Option<usize>,
    pub translation: [f32; 3],
    /// Quaternion as x, y, z, w.
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

impl Node {
    pub fn new(name: impl Into<String>, parent: Option<usize>) -> Self {
        Self {
            name: name.into(),
            parent,
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
        }
    }
}

/// A triangle list with one material. Optional attributes are empty when the source has none;
/// when present they have one entry per position.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mesh {
    pub name: String,
    /// The node the mesh is attached to. Ignored for skinned meshes, which follow their joints.
    pub node: usize,
    pub material: usize,
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    /// xyz tangent and w = ±1 for the bitangent's direction, as in glTF.
    pub tangents: Vec<[f32; 4]>,
    pub uvs: Vec<[f32; 2]>,
    /// Indices into the skin's joint list, with their weights; both set or both empty.
    pub joints: Vec<[u16; 4]>,
    pub weights: Vec<[f32; 4]>,
    pub skin: Option<usize>,
    pub indices: Vec<u32>,
}

/// Bones of a skinned mesh: the nodes it follows and their inverse bind matrices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Skin {
    pub joints: Vec<usize>,
    /// Column-major 4x4 matrices, one per joint.
    pub inverse_bind: Vec<[f32; 16]>,
}

/// A metallic-roughness PBR material.
#[derive(Clone, Debug, PartialEq)]
pub struct Material {
    pub name: String,
    pub base_color: Option<Image>,
    /// Multiplies `base_color`, linear RGBA.
    pub base_color_factor: [f32; 4],
    pub metallic_roughness: Option<Image>,
    pub metallic: f32,
    pub roughness: f32,
    pub normal: Option<Image>,
    pub emissive: Option<Image>,
    /// Multiplies `emissive`, linear RGB; may exceed 1.
    ///
    /// Units: relative, scene-referred radiance, the way glTF leaves it. 1.0 is reference white,
    /// i.e. as bright as an unlit white surface looks on an SDR display; values above 1 are the
    /// HDR range that bloom and tone mapping work with. It is not photometric: a renderer in
    /// physical units picks its own nits per 1.0 (HDR reference white is 203 cd/m² in ITU-R
    /// BT.2408), and lights, which are photometric (see [`Scene`]), are balanced against it by
    /// the renderer's exposure. The emissive image is sRGB; the light out is
    /// `emissive_factor * image` (times `emissive_strength`, if the model has one, see
    /// [`AnimatedModel::emissive_strength`]).
    pub emissive_factor: [f32; 3],
    pub alpha: AlphaMode,
    pub double_sided: bool,
}

impl Default for Material {
    fn default() -> Self {
        Self {
            name: String::new(),
            base_color: None,
            base_color_factor: [1.0; 4],
            metallic_roughness: None,
            metallic: 1.0,
            roughness: 1.0,
            normal: None,
            emissive: None,
            emissive_factor: [0.0; 3],
            alpha: AlphaMode::Opaque,
            double_sided: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AlphaMode {
    Opaque,
    /// Pixels with alpha below the cutoff are discarded.
    Mask(f32),
    Blend,
}

/// An 8-bit RGBA image, rows top to bottom.
#[derive(Clone, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl fmt::Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Image({}x{})", self.width, self.height)
    }
}

/// Why a model breaks the rules above.
#[derive(Clone, Debug, PartialEq)]
pub struct Invalid(pub String);

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for Invalid {}

impl Model {
    /// Checks that every index and attribute is consistent, so providers can catch mistakes
    /// before a renderer does.
    pub fn validate(&self) -> Result<(), Invalid> {
        let bad = |msg: String| Err(Invalid(format!("{}: {msg}", self.name)));
        for (i, n) in self.nodes.iter().enumerate() {
            if n.parent.is_some_and(|p| p >= i) {
                return bad(format!("node {i} ({}) comes before its parent", n.name));
            }
        }
        for (i, img) in self
            .materials
            .iter()
            .flat_map(|m| [&m.base_color, &m.metallic_roughness, &m.normal, &m.emissive])
            .flatten()
            .enumerate()
        {
            if img.rgba.len() as u64 != u64::from(img.width) * u64::from(img.height) * 4 {
                return bad(format!(
                    "image {i} is {}x{} but holds {} bytes",
                    img.width,
                    img.height,
                    img.rgba.len()
                ));
            }
        }
        for (i, s) in self.skins.iter().enumerate() {
            if s.inverse_bind.len() != s.joints.len() {
                return bad(format!(
                    "skin {i} has {} joints but {} inverse bind matrices",
                    s.joints.len(),
                    s.inverse_bind.len()
                ));
            }
            if let Some(&j) = s.joints.iter().find(|&&j| j >= self.nodes.len()) {
                return bad(format!(
                    "skin {i} names node {j}, past the {} nodes",
                    self.nodes.len()
                ));
            }
        }
        for (i, m) in self.meshes.iter().enumerate() {
            let n = m.positions.len();
            let mesh = |msg: String| bad(format!("mesh {i} ({}): {msg}", m.name));
            if m.material >= self.materials.len() {
                return mesh(format!("material {} out of range", m.material));
            }
            if m.skin.is_none() && m.node >= self.nodes.len() {
                return mesh(format!("node {} out of range", m.node));
            }
            for (attr, len) in [
                ("normals", m.normals.len()),
                ("tangents", m.tangents.len()),
                ("uvs", m.uvs.len()),
                ("joints", m.joints.len()),
                ("weights", m.weights.len()),
            ] {
                if len != 0 && len != n {
                    return mesh(format!("{len} {attr} for {n} positions"));
                }
            }
            if m.indices.len() % 3 != 0 {
                return mesh(format!(
                    "{} indices is not a whole number of triangles",
                    m.indices.len()
                ));
            }
            if let Some(&ix) = m.indices.iter().find(|&&ix| ix as usize >= n) {
                return mesh(format!("index {ix} past the {n} vertices"));
            }
            match m.skin {
                None if !m.joints.is_empty() => return mesh("joints without a skin".into()),
                Some(s) => {
                    let Some(skin) = self.skins.get(s) else {
                        return mesh(format!("skin {s} out of range"));
                    };
                    if m.joints.is_empty() || m.weights.is_empty() {
                        return mesh("skinned without joints and weights".into());
                    }
                    for (v, (j, w)) in m.joints.iter().zip(&m.weights).enumerate() {
                        if let Some(&j) = j
                            .iter()
                            .zip(w)
                            .find(|(_, &w)| w > 0.0)
                            .map(|(j, _)| j)
                            .filter(|&&j| usize::from(j) >= skin.joints.len())
                        {
                            return mesh(format!(
                                "vertex {v} uses joint {j}, past the skin's {}",
                                skin.joints.len()
                            ));
                        }
                        let sum: f32 = w.iter().sum();
                        if (sum - 1.0).abs() > 1e-3 {
                            return mesh(format!("vertex {v} weights sum to {sum}"));
                        }
                    }
                }
                None => {}
            }
        }
        Ok(())
    }

    /// Axis-aligned bounds of all positions in their mesh's own space, or `None` with no vertices.
    pub fn bounds(&self) -> Option<([f32; 3], [f32; 3])> {
        let mut it = self.meshes.iter().flat_map(|m| &m.positions);
        let first = *it.next()?;
        Some(it.fold((first, first), |(mut lo, mut hi), p| {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
            (lo, hi)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triangle() -> Model {
        Model {
            name: "tri".into(),
            nodes: vec![Node::new("root", None)],
            meshes: vec![Mesh {
                positions: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                uvs: vec![[0.0; 2]; 3],
                indices: vec![0, 1, 2],
                ..Mesh::default()
            }],
            materials: vec![Material::default()],
            skins: vec![],
        }
    }

    fn skinned() -> Model {
        let mut m = triangle();
        m.nodes.push(Node::new("bone", Some(0)));
        m.skins.push(Skin {
            joints: vec![0, 1],
            inverse_bind: vec![[0.0; 16]; 2],
        });
        let mesh = &mut m.meshes[0];
        mesh.skin = Some(0);
        mesh.joints = vec![[0, 1, 0, 0]; 3];
        mesh.weights = vec![[0.25, 0.75, 0.0, 0.0]; 3];
        m
    }

    #[test]
    fn valid_models_pass() {
        assert_eq!(triangle().validate(), Ok(()));
        assert_eq!(skinned().validate(), Ok(()));
        assert_eq!(triangle().bounds(), Some(([0.0; 3], [1.0, 1.0, 0.0])));
    }

    #[test]
    fn broken_models_are_reported() {
        let fails = |f: fn(&mut Model)| {
            let mut m = skinned();
            f(&mut m);
            m.validate().unwrap_err().0
        };
        assert!(fails(|m| m.meshes[0].indices[2] = 3).contains("index 3 past the 3 vertices"));
        assert!(fails(|m| {
            m.meshes[0].uvs.pop();
        })
        .contains("2 uvs for 3 positions"));
        assert!(fails(|m| m.nodes[1].parent = Some(1)).contains("comes before its parent"));
        assert!(
            fails(|m| m.meshes[0].weights[1] = [0.5, 0.0, 0.0, 0.0]).contains("weights sum to 0.5")
        );
        assert!(fails(|m| m.meshes[0].joints[0] = [2, 0, 0, 0]).contains("uses joint 2"));
        assert!(fails(|m| {
            m.skins[0].inverse_bind.pop();
        })
        .contains("2 joints but 1"));
        assert!(fails(|m| m.meshes[0].material = 1).contains("material 1 out of range"));
        assert!(fails(|m| m.materials[0].base_color = Some(Image {
            width: 2,
            height: 2,
            rgba: vec![0; 4]
        }))
        .contains("holds 4 bytes"));
    }

    #[test]
    fn unused_joint_slots_may_hold_anything() {
        let mut m = skinned();
        m.meshes[0].joints = vec![[1, 0, 9, 9]; 3];
        m.meshes[0].weights = vec![[1.0, 0.0, 0.0, 0.0]; 3];
        assert_eq!(m.validate(), Ok(()));
    }
}
