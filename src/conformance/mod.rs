//! A conformance checker: finds out whether a provider's output follows the bridge's conventions,
//! beyond what [`Model::validate`] can say.
//!
//! `validate` answers "is this structurally consistent?" (indices in range, attribute counts,
//! weights summing to 1, clips addressing real nodes with increasing times and unit
//! quaternions). Conformance starts from there: it calls `validate` first and reports a failure
//! as `structure.invalid`, then looks at what the data *means*: is the model the right size, does
//! it stand on the ground, do its triangles wind the way its normals say, do its bones bind,
//! does its clip spin a limb by flipping a quaternion's sign. These are the mistakes that
//! produce a valid but wrong-looking asset, such as a tank 100 times too large.
//!
//! ```
//! use rewrite_bridge::conformance::{check_model, Expect};
//! use rewrite_bridge::{Material, Mesh, Model, Node};
//!
//! let model = Model {
//!     name: "triangle".into(),
//!     nodes: vec![Node::new("root", None)],
//!     meshes: vec![Mesh {
//!         positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
//!         normals: vec![[0.0, 0.0, 1.0]; 3],
//!         indices: vec![0, 1, 2],
//!         ..Mesh::default()
//!     }],
//!     materials: vec![Material::default()],
//!     skins: vec![],
//! };
//! // Tell the checker what you know: this is a person, so 1.4 to 2.5 m tall.
//! let report = check_model(&model, &Expect::human());
//! assert!(report.passed());
//! assert!(report.has("scale.height_out_of_range")); // 1 m is short for a person
//! println!("{report}");
//! ```
//!
//! # Entry points
//!
//! - [`check_model`], [`check_animated`] and [`check_scene`] check one value and return a
//!   [`Report`];
//! - [`check_provider`] loads assets one at a time through a [`crate::Provider`] and checks each
//!   (streaming variant: [`check_provider_with`]), so a provider crate's tests can assert that
//!   its whole listing conforms;
//! - [`cli::run`] is a ready-made `check-conformance` subcommand for a provider's command line.
//!
//! [`Expect`] carries what the caller knows about the asset (it is a person; it is a tank) and
//! codes the caller knowingly differs on can be listed in [`Expect::allow`].
//!
//! # Finding codes
//!
//! Codes are stable. Severity is the default; a convention that games legitimately differ on is
//! `info`. "Why" is what goes wrong for a consumer.
//!
//! | code | severity | why it matters |
//! |------|----------|----------------|
//! | `structure.invalid` | error | `validate` failed; later checks need a consistent model, so nothing else runs |
//! | `scale.too_small` / `scale.too_large` | warning | metres is the contract; a model under [`SCALE_MIN_EXTENT_M`] or over [`SCALE_MAX_EXTENT_M`] is usually centimetres vs metres or a ×100 |
//! | `scale.height_out_of_range` / `scale.length_out_of_range` | warning | the caller's [`Expect`] range was missed, which a generic range cannot see |
//! | `geometry.no_vertices` | warning | nothing to draw, nothing to measure |
//! | `orientation.not_upright` / `orientation.wide` | warning / info | a character lying down means Z-up data read as Y-up |
//! | `orientation.feet_off_ground` | warning | characters are placed by their feet at y = 0 |
//! | `orientation.off_centre` | info | the origin is far from the middle of the model, which shifts rotation pivots |
//! | `front.not_plus_z` | warning | the front faces +Z; named front points (muzzle, eyes, face, headlight, plain front) or rear points (tail, plain rear) say otherwise |
//! | `geometry.non_finite` | error | NaN or infinity poisons bounds, culling and skinning |
//! | `geometry.empty_mesh` | warning | a mesh with vertices but no triangles draws nothing |
//! | `geometry.normals_not_unit` | warning | lighting assumes unit normals |
//! | `geometry.winding_flipped` | error | faces wind clockwise: back-face culling shows the inside |
//! | `geometry.winding_mixed` | info | part of the faces disagree with the normals: smoothing, or a partial flip |
//! | `geometry.degenerate_triangles` | warning / info | zero-area triangles waste work and break tangent generation |
//! | `geometry.unused_vertices` | info | dead weight in the vertex buffer |
//! | `geometry.no_normals` | info | the consumer has to invent flat normals |
//! | `skin.weights_non_finite` / `skin.negative_weights` / `skin.joint_out_of_range` | error | skinning reads garbage or explodes (`validate` rejects these since contract revision 4, so `check_model` reports them as `structure.invalid`; the codes stay for stability) |
//! | `skin.inverse_bind_non_finite` / `skin.inverse_bind_singular` | error | a collapsed or non-finite bind matrix flattens the mesh at its joint |
//! | `skin.inverse_bind_not_affine` | warning | the last row must be 0, 0, 0, 1 |
//! | `skin.bind_pose_mismatch` | info | the node pose is not the bind pose, so the model is deformed before any clip plays (some games bind that way) |
//! | `material.factor_invalid` / `material.emissive_invalid` | error | non-finite or negative factors |
//! | `material.base_color_factor_range` / `material.metallic_roughness_range` | warning | glTF factors are 0..=1 |
//! | `material.emissive_extreme` | warning | 1.0 is reference white; thousands is a unit mistake |
//! | `material.mask_cutoff_range` | warning | the cutoff compares with alpha in 0..=1 |
//! | `material.blend_opaque_texture` | warning | blending an opaque texture costs sorting for nothing |
//! | `material.opaque_transparent_texture` | warning | the transparent texels will show as solid colour |
//! | `material.unused` | info | a material no mesh uses |
//! | `image.empty` / `image.too_large` | error / warning | zero-sized (`validate` rejects it too, as `structure.invalid`), or larger than most GPUs accept |
//! | `image.normal_map_suspect` | warning | a normal map should be blue-dominant and centred on green and red 0.5; otherwise channels are swapped or it is not a tangent-space map |
//! | `animation.empty` / `animation.unnamed` / `animation.duplicate_name` | warning | the clip cannot be played or looked up by name; one finding per model, with the count |
//! | `animation.zero_duration` / `animation.duration_suspect` | warning | a clip of 0 s cannot loop; under 1 ms or over ten minutes is seconds vs milliseconds |
//! | `animation.quaternion_sign_flip` | warning | neighbouring keys in opposite hemispheres make some players spin the long way round |
//! | `animation.zero_scale` | warning | a zero scale key collapses the node and makes its matrix singular |
//! | `animation.root_motion` | info | slowest, median and fastest speed of the root over the model's clips, to eyeball units (a walk is 1 to 2 m/s) |
//! | `animation.root_motion_extreme` | warning | beyond [`ROOT_MOTION_EXTREME_MPS`] the clip is in the wrong units |
//! | `animation.pose_extent_mismatch` | warning | the animated skeleton's size differs a lot from the rest pose: clip and model disagree on units |
//! | `scene.empty` / `scene.unused_model` / `scene.duplicate_asset` | warning / info / info | nothing to draw; a model never placed; a model listed twice and loaded twice |
//! | `scene.asset_missing` | error | an instance names an asset the provider does not list |
//! | `scene.too_large` | warning | instances spread over more than [`SCENE_EXTENT_WARN_M`] lose float precision |
//! | `scene.instance_scale_extreme` | warning | an instance scaled outside 0.01 to 100 is usually a unit fix applied in the wrong place |
//! | `scene.nonuniform_scale` / `scene.negative_scale` | info | consumers must handle skewed normals and flipped winding |
//! | `provider.list_failed` | error | `Provider::list` itself failed |
//! | `provider.load_failed` / `provider.panicked` | error | the asset could not be loaded |
//! | `provider.scene_unsupported` | info | the game has no scenes |
//! | `provider.unlisted_id` | warning | an id that `Provider::list` does not give |
//! | `provider.load_mismatch` | error | `load(id)` differs from `load_animated(id)?.model`, which the trait asks for |
//!
//! No check looks at colour-space declarations, because [`crate::Image`] carries none: the
//! contract fixes which images are sRGB, so there is nothing to compare against.

mod animation;
mod geometry;
mod material;
mod math;
mod model;
mod provider;
mod report;
mod scene;

pub mod cli;

use crate::{AnimatedModel, AssetKind, Model, Scene};

pub use provider::{check_provider, check_provider_with, ProviderOptions};
pub use report::{tally, Facing, Finding, Location, Measurements, Report, Severity, Tally};

/// Largest extent (metres) below which a model is suspected to be in the wrong unit: 5 cm is
/// smaller than any asset a game ships except tiny props. Hints a ×0.01 mistake.
pub const SCALE_MIN_EXTENT_M: f32 = 0.05;
/// Largest extent (metres) above which a model is suspected to be in the wrong unit: 2 km is
/// larger than any single model except terrain. Hints a ×100 mistake or centimetres read as metres.
pub const SCALE_MAX_EXTENT_M: f32 = 2000.0;
/// How far (metres) the lowest point of a character may be from y = 0 before the feet are called
/// off the ground.
pub const FEET_TOLERANCE_M: f32 = 0.3;
/// A character whose height is below this fraction of its largest horizontal extent is lying down
/// (`orientation.not_upright`). A person exported Z-up and read as Y-up is about 0.15 to 0.25
/// (depth over height). Between this and 1 it is only reported as info, since a T-pose or a
/// four-legged animal is legitimately wider than tall; a long low creature (a rat with its tail,
/// a trilobite) can still fall below it and should be given `Expect::with_character(false)`.
pub const UPRIGHT_MIN_RATIO: f32 = 0.25;
/// Distance (metres) between the bounding box centre and the origin, in X or Z, above which it is
/// reported as info.
pub const CENTRE_OFFSET_REPORT_M: f32 = 0.1;
/// Largest |length - 1| of a normal before it counts as not unit length.
pub const NORMAL_TOLERANCE: f32 = 1e-3;
/// Fraction of non-degenerate triangles whose face normal disagrees with their vertex normals
/// above which the mesh is called flipped.
pub const WINDING_FLIPPED_FRACTION: f64 = 0.5;
/// Same, above which the mesh is reported as mixed (info).
pub const WINDING_MIXED_FRACTION: f64 = 0.1;
/// Cosine between face normal and vertex normals within which a triangle counts as neither agreeing
/// nor disagreeing (a triangle seen edge-on).
pub const WINDING_AMBIGUOUS_COS: f64 = 0.01;
/// A triangle is degenerate if twice its area is below this fraction of its longest edge squared.
pub const DEGENERATE_AREA_RATIO: f64 = 1e-9;
/// Fraction of degenerate triangles above which `geometry.degenerate_triangles` is a warning
/// rather than info.
pub const DEGENERATE_WARN_FRACTION: f64 = 0.01;
/// Largest element of `world(joint) * inverse_bind` away from the identity before a joint is said
/// not to be bound in the node pose (`skin.bind_pose_mismatch`).
pub const BIND_POSE_TOLERANCE: f64 = 1e-2;
/// Smallest |det| of an inverse bind matrix's 3x3 part, divided by the product of its column
/// lengths, that still counts as invertible.
pub const INVERSE_BIND_MIN_DET: f64 = 1e-6;
/// Emissive factor above which a material is flagged; 1.0 is reference white.
pub const EMISSIVE_EXTREME: f32 = 100.0;
/// Largest image side (pixels) most GPUs accept.
pub const IMAGE_MAX_SIDE: u32 = 16384;
/// Fraction of texels with alpha below 128 above which an opaque material is suspicious.
pub const OPAQUE_TRANSPARENT_FRACTION: f64 = 0.05;
/// Mean blue (0..=1) below which a normal map is suspicious.
pub const NORMAL_MAP_MIN_MEAN_BLUE: f64 = 0.5;
/// How far (0..=1) the mean red and green of a normal map may be from 0.5.
pub const NORMAL_MAP_MAX_XY_OFFSET: f64 = 0.15;
/// A clip shorter than this many seconds is suspected to be in milliseconds read as seconds.
pub const ANIMATION_MIN_DURATION_S: f32 = 1e-3;
/// A clip longer than this many seconds is suspected to be in milliseconds read as seconds.
pub const ANIMATION_MAX_DURATION_S: f32 = 600.0;
/// Root motion (metres per second) below which `animation.root_motion` is not reported.
pub const ROOT_MOTION_REPORT_MPS: f64 = 0.1;
/// Root motion (metres per second) above which a clip is called extreme. A sprinting human is 10,
/// a fast car 70; 100 is a unit bug.
pub const ROOT_MOTION_EXTREME_MPS: f64 = 100.0;
/// Ratio of the animated skeleton's extent to the rest pose's, beyond which (or its inverse)
/// `animation.pose_extent_mismatch` fires.
pub const POSE_EXTENT_RATIO: f64 = 4.0;
/// Extent (metres) of instance origins in a scene beyond which `scene.too_large` fires; 32-bit
/// floats resolve about 8 mm at 100 km.
pub const SCENE_EXTENT_WARN_M: f32 = 100_000.0;
/// Instance scale outside `[1/N, N]` with N = this triggers `scene.instance_scale_extreme`.
pub const INSTANCE_SCALE_EXTREME: f32 = 100.0;
/// Relative difference between the largest and smallest |scale| of an instance above which it is
/// non-uniform.
pub const NONUNIFORM_SCALE_TOLERANCE: f32 = 1e-3;

/// What the caller knows about the asset. Everything is optional; the default is generic and
/// asserts nothing about what the asset is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Expect {
    /// Height (extent in Y) in metres the asset should have, inclusive.
    pub height_m: Option<(f32, f32)>,
    /// Length (the larger of the extents in X and Z) in metres the asset should have, inclusive.
    pub length_m: Option<(f32, f32)>,
    /// `Some(true)` runs the character checks (upright, feet on the ground) even without a skin,
    /// `Some(false)` skips them even with one, `None` treats a model with a skin as a character.
    pub is_character: Option<bool>,
    /// Finding codes to drop from the report: conventions this game knowingly differs on.
    pub allow: Vec<String>,
}

impl Expect {
    /// Generic: no ranges, characters recognised by their skin.
    pub fn generic() -> Self {
        Self::default()
    }

    /// A person: 1.4 to 2.5 m tall, a character.
    pub fn human() -> Self {
        Self {
            height_m: Some((1.4, 2.5)),
            is_character: Some(true),
            ..Self::default()
        }
    }

    /// A tank: 2 to 4 m tall and 4 to 10 m long.
    pub fn tank() -> Self {
        Self {
            height_m: Some((2.0, 4.0)),
            length_m: Some((4.0, 10.0)),
            is_character: Some(false),
            ..Self::default()
        }
    }

    /// What can be said of an asset from its kind alone: a character is a character and between
    /// 0.2 and 6 m tall (a mouse-sized creature to a dragon); vehicles, weapons, props and levels
    /// are not characters, so a skeleton in them does not make them stand upright with their feet
    /// on the ground; `Other` is generic.
    pub fn for_kind(kind: AssetKind) -> Self {
        match kind {
            AssetKind::Character => Self {
                height_m: Some((0.2, 6.0)),
                is_character: Some(true),
                ..Self::default()
            },
            AssetKind::Other => Self::default(),
            _ => Self {
                is_character: Some(false),
                ..Self::default()
            },
        }
    }

    pub fn with_height(mut self, min: f32, max: f32) -> Self {
        self.height_m = Some((min, max));
        self
    }

    pub fn with_length(mut self, min: f32, max: f32) -> Self {
        self.length_m = Some((min, max));
        self
    }

    pub fn with_character(mut self, is_character: bool) -> Self {
        self.is_character = Some(is_character);
        self
    }

    /// Drops findings with this code from every report made with this `Expect`.
    pub fn allowing(mut self, code: &str) -> Self {
        self.allow.push(code.to_string());
        self
    }
}

/// Checks a model: [`Model::validate`], then scale, orientation, geometry, skins and materials.
pub fn check_model(model: &Model, expect: &Expect) -> Report {
    let mut report = Report::new(model.name.clone());
    if let Err(e) = model.validate() {
        report.push(Severity::Error, "structure.invalid", Location::none(), e.0);
    } else {
        model::check(model, &[], expect, &mut report);
    }
    report.allow(&expect.allow);
    report
}

/// Checks a model with its clips, sockets and emissive strengths: [`AnimatedModel::validate`],
/// everything [`check_model`] checks, then the animation checks.
pub fn check_animated(animated: &AnimatedModel, expect: &Expect) -> Report {
    let mut report = Report::new(animated.model.name.clone());
    if let Err(e) = animated.validate() {
        report.push(Severity::Error, "structure.invalid", Location::none(), e.0);
    } else {
        model::check(&animated.model, &animated.sockets, expect, &mut report);
        animation::check(animated, &mut report);
        report.measured.clips = animated.animations.len();
    }
    report.allow(&expect.allow);
    report
}

/// Checks a scene: [`Scene::validate`], then its inline models, instances and extent. Whether
/// asset references resolve needs a provider; [`check_provider`] does that.
pub fn check_scene(scene: &Scene, expect: &Expect) -> Report {
    let mut report = Report::new(scene.name.clone());
    if let Err(e) = scene.validate() {
        report.push(Severity::Error, "structure.invalid", Location::none(), e.0);
    } else {
        scene::check(scene, expect, &mut report);
    }
    report.allow(&expect.allow);
    report
}

#[cfg(test)]
mod tests;
