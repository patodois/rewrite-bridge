//! Scenes: levels made of models placed many times.
//!
//! A [`Scene`] holds a list of distinct models and a list of [`Instance`]s, each saying "model `n`
//! stands here". A forest of ten thousand trees is a handful of tree models and ten thousand
//! 40-byte instances, so no mesh is duplicated. A model is either inline ([`SceneModel::Inline`])
//! or a reference ([`SceneModel::Asset`]) that the consumer loads lazily through the provider
//! with [`crate::Provider::load`] (or [`crate::Provider::load_animated`]), once per model and not
//! once per instance.
//!
//! The scene is in world space: metres, +Y up, right-handed, as for models. An instance's
//! [`Transform`] takes the model's own space (its root nodes' transforms still apply inside it)
//! to the world. Instances have no parent/child structure and no draw order.
//!
//! # Lights
//!
//! Lights are optional and a provider fills them in only if the game has real ones. Their
//! intensities use the physical units of glTF's `KHR_lights_punctual`:
//!
//! | kind | unit | meaning |
//! |------|------|---------|
//! | directional | lux (lm/m²) | illuminance on a surface facing the light; bright sun is about 100 000 lux |
//! | point | candela (lm/sr) | luminous intensity; a 100 W incandescent bulb is about 135 cd (1 700 lm) |
//! | spot | candela (lm/sr) | luminous intensity along the axis, whatever the cone angle |
//!
//! A source specified in lumens Φ becomes candela by dividing by 4π (point and spot alike); there
//! is no lumens field so that there is one way to say each thing. A light shines along its local
//! -Z axis, as in glTF, and the light's transform places and aims it (scale is ignored, and so is
//! the translation of a directional light).
//!
//! Lights are photometric while emissive materials and the environment are relative
//! (see [`crate::Material::emissive_factor`]); a renderer that mixes them chooses its exposure.
//!
//! # Environment
//!
//! The optional [`Environment`] is the sky seen behind everything: a flat colour or an
//! equirectangular picture.

use crate::{Image, Invalid, Model, Transform};

/// A level: models, where they stand, and optionally lights and a sky.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene {
    pub name: String,
    /// The distinct models; [`Instance::model`] indexes this.
    pub models: Vec<SceneModel>,
    /// Placements of those models.
    pub instances: Vec<Instance>,
    /// Empty unless the game provides lights.
    pub lights: Vec<Light>,
    /// `None` unless the game provides a sky.
    pub environment: Option<Environment>,
}

/// A model of a scene, held or by reference.
///
/// Matches may need a wildcard arm: more ways to hold a model (for instance an
/// [`crate::AnimatedModel`]) can be added later without a breaking change.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum SceneModel {
    /// The whole model, right here.
    Inline(Model),
    /// A model to load later through a provider.
    Asset(AssetRef),
}

/// A provider's asset, named the way the passthrough link's `spawn` message names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetRef {
    /// The game, as [`crate::Provider::game`] returns it; `None` means the provider that gave
    /// this scene. Another game's name makes a mash-up scene.
    pub provider: Option<String>,
    /// An id [`crate::Provider::list`] gave for the asset.
    pub asset: String,
}

impl AssetRef {
    /// An asset of the provider that returned the scene.
    pub fn local(asset: impl Into<String>) -> Self {
        Self {
            provider: None,
            asset: asset.into(),
        }
    }

    /// An asset of another game's provider.
    pub fn of(provider: impl Into<String>, asset: impl Into<String>) -> Self {
        Self {
            provider: Some(provider.into()),
            asset: asset.into(),
        }
    }
}

/// One placement of a scene model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Instance {
    /// Index into [`Scene::models`].
    pub model: usize,
    /// Model space to world space.
    pub transform: Transform,
}

impl Instance {
    pub fn new(model: usize, transform: Transform) -> Self {
        Self { model, transform }
    }
}

/// A punctual light.
#[derive(Clone, Debug, PartialEq)]
pub struct Light {
    pub name: String,
    pub kind: LightKind,
    /// Linear RGB, each in 0..=1.
    pub color: [f32; 3],
    /// Lux for a directional light, candela for the others; see the module docs. Not negative.
    pub intensity: f32,
    /// World placement; the light shines along local -Z. Scale is ignored.
    pub transform: Transform,
}

/// What shape of light, with the parameters only that shape has.
///
/// Matches may need a wildcard arm: more kinds can be added later without a breaking change.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum LightKind {
    /// Parallel rays, like the sun: only the rotation of the transform matters.
    Directional,
    Point {
        /// Metres beyond which the light is cut off; `None` for no limit.
        range: Option<f32>,
    },
    Spot {
        /// Metres beyond which the light is cut off; `None` for no limit.
        range: Option<f32>,
        /// Radians from the axis within which the light is at full intensity.
        inner_cone_angle: f32,
        /// Radians from the axis at which the light reaches zero; at most π/2, above the inner.
        outer_cone_angle: f32,
    },
}

/// The sky behind the scene.
///
/// Matches may need a wildcard arm: more kinds (a cube map, say) can be added later without a
/// breaking change.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Environment {
    /// A flat colour in every direction: linear RGB, relative scene-referred radiance on the same
    /// scale as `Material::emissive_factor` (1.0 is reference white; may exceed 1).
    Color([f32; 3]),
    /// A 360° picture, 8-bit sRGB, rows top to bottom, covering the full sphere.
    ///
    /// For a view direction `d` (+Y up, right-handed, the viewer looking down -Z at the picture's
    /// middle) the texel is at `u = 0.5 + atan2(d.x, -d.z) / 2π` and `v = 0.5 - asin(d.y) / π`,
    /// with `u` across the width from the left edge and `v` down the height from the top edge.
    /// So the middle column is -Z, turning right (toward +X) moves right in the picture, and the
    /// top row is straight up. The linearised texel is multiplied by `intensity`, which is on the
    /// same scale as the colour above.
    Equirectangular { image: Image, intensity: f32 },
}

impl Scene {
    /// Checks the scene:
    ///
    /// - inline models pass [`Model::validate`]; asset references have a non-empty asset id and,
    ///   if they name a provider, a non-empty name;
    /// - every instance names a model in range and has a valid [`Transform`] (no zero scale, a
    ///   unit-length rotation);
    /// - lights have colours in 0..=1, a finite non-negative intensity, a valid transform, a
    ///   positive range if any, and for spots `0 <= inner < outer <= π/2`;
    /// - an environment colour or intensity is finite and not negative, and a picture's byte
    ///   count is `width * height * 4` with both sides non-zero.
    pub fn validate(&self) -> Result<(), Invalid> {
        let bad = |msg: String| Err(Invalid(format!("{}: {msg}", self.name)));
        for (i, m) in self.models.iter().enumerate() {
            match m {
                SceneModel::Inline(model) => {
                    if let Err(e) = model.validate() {
                        return bad(format!("model {i}: {}", e.0));
                    }
                }
                SceneModel::Asset(a) => {
                    if a.asset.is_empty() {
                        return bad(format!("model {i}: asset reference has no id"));
                    }
                    if a.provider.as_deref() == Some("") {
                        return bad(format!(
                            "model {i}: asset {:?} names an empty provider",
                            a.asset
                        ));
                    }
                }
            }
        }
        for (i, inst) in self.instances.iter().enumerate() {
            if inst.model >= self.models.len() {
                return bad(format!(
                    "instance {i}: model {} out of range of {}",
                    inst.model,
                    self.models.len()
                ));
            }
            if let Some(p) = inst.transform.problem() {
                return bad(format!("instance {i}: {p}"));
            }
        }
        for (i, l) in self.lights.iter().enumerate() {
            if let Some(p) = l.problem() {
                return bad(format!("light {i} ({}): {p}", l.name));
            }
        }
        match &self.environment {
            Some(Environment::Color(c)) if c.iter().any(|c| !c.is_finite() || *c < 0.0) => {
                return bad(format!(
                    "environment colour {c:?} is negative or not finite"
                ));
            }
            Some(Environment::Equirectangular { image, intensity }) => {
                if !intensity.is_finite() || *intensity < 0.0 {
                    return bad(format!(
                        "environment intensity {intensity} is negative or not finite"
                    ));
                }
                if image.width == 0
                    || image.height == 0
                    || image.rgba.len() as u64
                        != u64::from(image.width) * u64::from(image.height) * 4
                {
                    return bad(format!(
                        "environment picture is {}x{} but holds {} bytes",
                        image.width,
                        image.height,
                        image.rgba.len()
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl Light {
    fn problem(&self) -> Option<String> {
        if self.color.iter().any(|c| !(0.0..=1.0).contains(c)) {
            return Some(format!("colour {:?} is outside 0..=1", self.color));
        }
        if !self.intensity.is_finite() || self.intensity < 0.0 {
            return Some(format!(
                "intensity {} is negative or not finite",
                self.intensity
            ));
        }
        if let Some(p) = self.transform.problem() {
            return Some(p);
        }
        let range = match self.kind {
            LightKind::Directional => None,
            LightKind::Point { range } => range,
            LightKind::Spot { range, .. } => range,
        };
        if range.is_some_and(|r| !r.is_finite() || r <= 0.0) {
            return Some(format!("range {range:?} is not a positive length"));
        }
        if let LightKind::Spot {
            inner_cone_angle: i,
            outer_cone_angle: o,
            ..
        } = self.kind
        {
            if !(0.0 <= i && i < o && o <= std::f32::consts::FRAC_PI_2) {
                return Some(format!(
                    "cone angles inner {i} and outer {o} are not 0 <= inner < outer <= pi/2"
                ));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Node;
    use std::f32::consts::FRAC_PI_2;

    fn tree() -> Model {
        Model {
            name: "tree".into(),
            nodes: vec![Node::new("root", None)],
            ..Model::default()
        }
    }

    fn scene() -> Scene {
        Scene {
            name: "forest".into(),
            models: vec![
                SceneModel::Inline(tree()),
                SceneModel::Asset(AssetRef::local("house/small")),
                SceneModel::Asset(AssetRef::of("Other Game", "rock")),
            ],
            instances: vec![
                Instance::new(0, Transform::from_translation([10.0, 0.0, 5.0])),
                Instance::new(0, Transform::from_translation([-3.0, 0.0, 8.0])),
                Instance::new(1, Transform::IDENTITY),
                Instance::new(2, Transform::IDENTITY),
            ],
            lights: vec![
                Light {
                    name: "sun".into(),
                    kind: LightKind::Directional,
                    color: [1.0, 0.95, 0.9],
                    intensity: 100_000.0,
                    transform: Transform::IDENTITY,
                },
                Light {
                    name: "lamp".into(),
                    kind: LightKind::Point { range: Some(20.0) },
                    color: [1.0; 3],
                    intensity: 135.0,
                    transform: Transform::from_translation([0.0, 3.0, 0.0]),
                },
                Light {
                    name: "torch".into(),
                    kind: LightKind::Spot {
                        range: None,
                        inner_cone_angle: 0.2,
                        outer_cone_angle: 0.5,
                    },
                    color: [1.0; 3],
                    intensity: 500.0,
                    transform: Transform::IDENTITY,
                },
            ],
            environment: Some(Environment::Color([0.3, 0.5, 0.9])),
        }
    }

    fn fails(f: impl FnOnce(&mut Scene)) -> String {
        let mut s = scene();
        f(&mut s);
        s.validate().unwrap_err().0
    }

    fn picture(w: u32, h: u32, bytes: usize) -> Environment {
        Environment::Equirectangular {
            image: Image {
                width: w,
                height: h,
                rgba: vec![0; bytes],
            },
            intensity: 1.0,
        }
    }

    #[test]
    fn valid_scenes_pass() {
        assert_eq!(scene().validate(), Ok(()));
        assert_eq!(Scene::default().validate(), Ok(()));
        let mut s = scene();
        s.environment = Some(picture(2, 1, 8));
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn inline_models_are_validated() {
        let msg = fails(|s| {
            if let SceneModel::Inline(m) = &mut s.models[0] {
                m.nodes.push(Node::new("bad", Some(5)));
            }
        });
        assert!(
            msg.contains("forest: model 0: tree: node 1 (bad) comes before its parent"),
            "{msg}"
        );
    }

    #[test]
    fn asset_references_are_validated() {
        assert!(
            fails(|s| s.models[1] = SceneModel::Asset(AssetRef::local("")))
                .contains("model 1: asset reference has no id")
        );
        assert!(
            fails(|s| s.models[2] = SceneModel::Asset(AssetRef::of("", "rock")))
                .contains("names an empty provider")
        );
    }

    #[test]
    fn instances_are_validated() {
        assert!(
            fails(|s| s.instances[1].model = 3).contains("instance 1: model 3 out of range of 3")
        );
        assert!(fails(|s| s.instances[0].transform.scale = [1.0, 0.0, 1.0])
            .contains("instance 0: scale"));
        assert!(fails(|s| s.instances[0].transform.rotation = [1.0; 4])
            .contains("instance 0: quaternion"));
        assert!(
            fails(|s| s.instances[0].transform.translation[0] = f32::NAN)
                .contains("instance 0: translation")
        );
    }

    #[test]
    fn lights_are_validated() {
        assert!(fails(|s| s.lights[0].color[1] = 1.5).contains("light 0 (sun): colour"));
        assert!(fails(|s| s.lights[1].intensity = -1.0).contains("light 1 (lamp): intensity -1"));
        assert!(fails(|s| s.lights[1].intensity = f32::INFINITY).contains("intensity"));
        assert!(fails(|s| s.lights[0].transform.rotation = [0.0; 4])
            .contains("light 0 (sun): quaternion"));
        assert!(
            fails(|s| s.lights[1].kind = LightKind::Point { range: Some(0.0) })
                .contains("range Some(0.0) is not a positive length")
        );
        let spot = |i, o| LightKind::Spot {
            range: None,
            inner_cone_angle: i,
            outer_cone_angle: o,
        };
        assert!(fails(|s| s.lights[2].kind = spot(0.5, 0.5)).contains("cone angles"));
        assert!(fails(|s| s.lights[2].kind = spot(-0.1, 0.5)).contains("cone angles"));
        assert!(fails(|s| s.lights[2].kind = spot(0.1, FRAC_PI_2 + 0.01)).contains("cone angles"));
        let mut ok = scene();
        ok.lights[2].kind = spot(0.0, FRAC_PI_2);
        assert_eq!(ok.validate(), Ok(()));
    }

    #[test]
    fn environments_are_validated() {
        assert!(
            fails(|s| s.environment = Some(Environment::Color([0.0, -1.0, 0.0])))
                .contains("environment colour")
        );
        assert!(fails(|s| s.environment = Some(picture(2, 2, 4))).contains("2x2 but holds 4 bytes"));
        assert!(fails(|s| s.environment = Some(picture(0, 0, 0))).contains("0x0 but holds 0 bytes"));
        assert!(fails(|s| {
            s.environment = Some(Environment::Equirectangular {
                image: Image {
                    width: 1,
                    height: 1,
                    rgba: vec![0; 4],
                },
                intensity: -1.0,
            })
        })
        .contains("environment intensity -1"));
        // A bright sky is fine: colours may exceed 1.
        let mut s = scene();
        s.environment = Some(Environment::Color([8.0, 8.0, 8.0]));
        assert_eq!(s.validate(), Ok(()));
    }
}
