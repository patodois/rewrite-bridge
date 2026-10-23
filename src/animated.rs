//! A [`Model`] together with what a static model cannot say: animation clips, attachment points
//! and emissive strengths.
//!
//! [`Model`], [`crate::Mesh`] and [`crate::Material`] are plain structs that other crates build
//! with struct literals, so they cannot gain fields without breaking those crates. Everything new
//! therefore lives here, next to the model instead of inside it. A provider that has none of it
//! returns just a `Model` from [`crate::Provider::load`]; one that has some overrides
//! [`crate::Provider::load_animated`].
//!
//! # Attachment points
//!
//! A [`Socket`] is a named place on a model where something else (a weapon in a hand, a rider on
//! a seat, a flame on a muzzle) can be attached. It is a node plus an offset from that node, so
//! the socket follows the node through every animation. Names are the provider's own; the crate
//! defines none.
//!
//! Sockets are a separate list, rather than a convention that "nodes named like X are sockets",
//! for three reasons: a naming convention cannot tell a socket from a bone that happens to share
//! the name; the offset would need an extra node, changing `nodes` indices that skins and
//! animations already point at; and a list can be validated and looked up by name without
//! scanning node names.
//!
//! # Emissive strength
//!
//! See [`AnimatedModel::emissive_strength`].

use std::collections::HashSet;

use crate::{Animation, Invalid, Model, Transform};

/// A named attachment point: node `node`'s world transform, then `offset` on top of it.
///
/// The attached object receives `world(node) * offset` as its own world transform, scale
/// included, so a socket on a scaled node scales what is attached to it. A provider that wants
/// an unscaled socket puts it on an unscaled node or divides the scale out in `offset`.
#[derive(Clone, Debug, PartialEq)]
pub struct Socket {
    /// Unique within the model; chosen by the provider.
    pub name: String,
    /// Index into the model's `nodes`. It may be a joint of a skin.
    pub node: usize,
    /// Placement relative to the node, in the node's own space (metres).
    pub offset: Transform,
}

impl Socket {
    pub fn new(name: impl Into<String>, node: usize, offset: Transform) -> Self {
        Self {
            name: name.into(),
            node,
            offset,
        }
    }
}

/// A model plus animation clips, attachment points and per-material emissive strengths.
///
/// `AnimatedModel::from(model)` is a model with none of those, which is what
/// [`crate::Provider::load_animated`] returns for providers that do not override it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AnimatedModel {
    pub model: Model,
    /// Clips that animate `model.nodes`. Empty for a static model.
    pub animations: Vec<Animation>,
    /// Attachment points on `model.nodes`.
    pub sockets: Vec<Socket>,
    /// Optional `KHR_materials_emissive_strength`: either empty (every material has strength 1)
    /// or one entry per material in `model.materials`.
    ///
    /// The light a material emits is `emissive_factor * emissive_strength` (times the emissive
    /// image, if any); see [`AnimatedModel::emissive_factor`]. The strength is a separate number
    /// so a glTF with factors in 0..=1 and a strength above 1 survives a round trip untouched;
    /// providers that don't care can simply multiply it into `Material::emissive_factor`, which
    /// is already allowed to exceed 1, and leave this empty. Values are not negative.
    ///
    /// It lives here, not on `Material`, because adding a field there would break every struct
    /// literal of `Material` in the crates that depend on this one.
    pub emissive_strength: Vec<f32>,
}

impl From<Model> for AnimatedModel {
    fn from(model: Model) -> Self {
        Self {
            model,
            ..Self::default()
        }
    }
}

impl AnimatedModel {
    pub fn new(model: Model) -> Self {
        model.into()
    }

    /// The socket with this name.
    pub fn socket(&self, name: &str) -> Option<&Socket> {
        self.sockets.iter().find(|s| s.name == name)
    }

    /// The clip with this name (the first, if names repeat).
    pub fn animation(&self, name: &str) -> Option<&Animation> {
        self.animations.iter().find(|a| a.name == name)
    }

    /// A material's emissive factor with its strength applied: `emissive_factor *
    /// emissive_strength`, linear RGB, relative scene-referred radiance (see
    /// [`crate::Material::emissive_factor`]). `None` if there is no such material.
    pub fn emissive_factor(&self, material: usize) -> Option<[f32; 3]> {
        let f = self.model.materials.get(material)?.emissive_factor;
        let s = self.emissive_strength.get(material).copied().unwrap_or(1.0);
        Some(f.map(|c| c * s))
    }

    /// [`Model::validate`], plus: every animation passes [`Animation::validate`] for this model's
    /// nodes; sockets name a node in range, have a non-empty, unique name and a valid offset
    /// ([`Transform::validate`]); and `emissive_strength` is empty or has one finite,
    /// non-negative entry per material.
    pub fn validate(&self) -> Result<(), Invalid> {
        self.model.validate()?;
        let bad = |msg: String| Err(Invalid(format!("{}: {msg}", self.model.name)));
        let nodes = self.model.nodes.len();
        for (i, a) in self.animations.iter().enumerate() {
            if let Err(e) = a.validate(nodes) {
                return bad(format!("animation {i}: {}", e.0));
            }
        }
        let mut names = HashSet::new();
        for (i, s) in self.sockets.iter().enumerate() {
            let sock = |msg: String| bad(format!("socket {i} ({}): {msg}", s.name));
            if s.name.is_empty() {
                return sock("has no name".into());
            }
            if !names.insert(s.name.as_str()) {
                return sock("name is used twice".into());
            }
            if s.node >= nodes {
                return sock(format!("node {} out of range of {nodes}", s.node));
            }
            if let Some(p) = s.offset.problem() {
                return sock(p);
            }
        }
        if !self.emissive_strength.is_empty() {
            let n = self.model.materials.len();
            if self.emissive_strength.len() != n {
                return bad(format!(
                    "{} emissive strengths for {n} materials",
                    self.emissive_strength.len()
                ));
            }
            if let Some(s) = self
                .emissive_strength
                .iter()
                .find(|s| !s.is_finite() || **s < 0.0)
            {
                return bad(format!("emissive strength {s} is negative or not finite"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Channel, Interpolation, Material, Node, Target};

    fn animated() -> AnimatedModel {
        let mut m = AnimatedModel::new(Model {
            name: "robot".into(),
            nodes: vec![Node::new("root", None), Node::new("hand", Some(0))],
            materials: vec![
                Material {
                    emissive_factor: [0.5, 0.25, 0.0],
                    ..Material::default()
                },
                Material::default(),
            ],
            ..Model::default()
        });
        m.animations.push(Animation {
            name: "wave".into(),
            duration_s: 1.0,
            channels: vec![Channel::new(
                1,
                Target::Translation,
                Interpolation::Linear,
                vec![0.0, 1.0],
                vec![0.0, 0.0, 0.0, 0.0, 1.0, 0.0],
            )],
        });
        m.sockets.push(Socket::new(
            "grip",
            1,
            Transform::from_translation([0.0, 0.1, 0.0]),
        ));
        m.emissive_strength = vec![4.0, 1.0];
        m
    }

    fn fails(f: impl FnOnce(&mut AnimatedModel)) -> String {
        let mut m = animated();
        f(&mut m);
        m.validate().unwrap_err().0
    }

    #[test]
    fn valid_models_pass_and_lookups_work() {
        let m = animated();
        assert_eq!(m.validate(), Ok(()));
        assert_eq!(m.socket("grip").map(|s| s.node), Some(1));
        assert!(m.socket("nope").is_none());
        assert_eq!(m.animation("wave").map(|a| a.channels.len()), Some(1));
        assert_eq!(AnimatedModel::default().validate(), Ok(()));
        assert_eq!(AnimatedModel::from(m.model.clone()).validate(), Ok(()));
    }

    #[test]
    fn the_model_is_validated_too() {
        assert!(fails(|m| m.model.nodes[1].parent = Some(1)).contains("comes before its parent"));
    }

    #[test]
    fn animations_are_validated_against_the_nodes() {
        let msg = fails(|m| m.animations[0].channels[0].node = 2);
        assert!(
            msg.contains("robot: animation 0: wave: channel 0: node 2 out of range of 2"),
            "{msg}"
        );
    }

    #[test]
    fn sockets_are_validated() {
        assert!(fails(|m| m.sockets[0].name.clear()).contains("has no name"));
        assert!(fails(|m| m.sockets[0].node = 2).contains("node 2 out of range of 2"));
        assert!(fails(|m| m.sockets[0].offset.rotation = [0.0; 4]).contains("quaternion"));
        assert!(fails(|m| m.sockets[0].offset.scale = [0.0; 3]).contains("scale"));
        assert!(fails(|m| {
            let dup = m.sockets[0].clone();
            m.sockets.push(dup);
        })
        .contains("socket 1 (grip): name is used twice"));
    }

    #[test]
    fn emissive_strengths_are_validated() {
        assert!(fails(|m| {
            m.emissive_strength.pop();
        })
        .contains("1 emissive strengths for 2 materials"));
        assert!(fails(|m| m.emissive_strength[0] = -1.0).contains("negative or not finite"));
        assert!(fails(|m| m.emissive_strength[1] = f32::NAN).contains("negative or not finite"));
        // Empty means 1 for everyone.
        let mut m = animated();
        m.emissive_strength.clear();
        assert_eq!(m.validate(), Ok(()));
    }

    #[test]
    fn emissive_factor_applies_the_strength() {
        let mut m = animated();
        assert_eq!(m.emissive_factor(0), Some([2.0, 1.0, 0.0]));
        assert_eq!(m.emissive_factor(1), Some([0.0; 3]));
        assert_eq!(m.emissive_factor(2), None);
        m.emissive_strength.clear();
        assert_eq!(m.emissive_factor(0), Some([0.5, 0.25, 0.0]));
    }
}
