//! Static and skinned models: bridge [`rb::Model`] to meshes, materials and an entity hierarchy.
//!
//! [`spawn_model`] turns a model into:
//!
//! - a root entity named after the model, with every root node of the model as its child;
//! - one entity per [`rewrite_bridge::Node`], in the same hierarchy, carrying the node's rest
//!   transform and a [`BridgeNode`] component with its index;
//! - one [`Mesh3d`] entity per [`rewrite_bridge::Mesh`]: rigid meshes hang under their node, skinned
//!   meshes under the root with a [`SkinnedMesh`] whose joints are the node entities;
//! - one [`StandardMaterial`] per [`rewrite_bridge::Material`], shared by the meshes that use it.
//!
//! Pose a model by changing the [`Transform`] of its node entities: a skinned mesh follows its joint
//! entities, and rigid meshes follow their node. [`crate::spawn_animated`] does that from clips.
//!
//! Geometry and images are uploaded with [`RenderAssetUsages::RENDER_WORLD`] by default, so the CPU
//! copies are freed once they reach the GPU. [`spawn_model_owned`] moves the model's pixels and vertices
//! into Bevy instead of copying them, for large models; keep a copy of the model if the CPU side needs
//! the data later.
//!
//! [`rest_globals`] and [`rest_bounds`] work on the model alone (no Bevy world), e.g. to measure a
//! model or place one model on another before spawning.

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::{DynamicSkinnedMeshBounds, NoFrustumCulling};
use bevy::camera::Exposure;
use bevy::ecs::system::SystemParam;
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::mesh::skinning::{SkinnedMesh, SkinnedMeshInverseBindposes};
use bevy::mesh::{Indices, PrimitiveTopology, VertexAttributeValues};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, Face, TextureDimension, TextureFormat};
use rewrite_bridge as rb;

/// The luminance in cd/m² that a camera with this exposure shows as white (1.0 before tone mapping):
/// `1.2 * 2^ev100`, the inverse of [`Exposure::exposure`]. It is what a relative value of 1.0 (reference
/// white in the bridge's emissive factors and sky) must be multiplied by to look white under that
/// exposure when it goes through the exposure.
pub fn white_luminance(exposure: Exposure) -> f32 {
    1.0 / exposure.exposure()
}

/// Joints Bevy's skinning shader accepts per skinned mesh
pub const MAX_JOINTS: usize = 256;

/// How skinned meshes get the bounds Bevy culls them with
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SkinnedBounds {
    /// Bounds follow the joints every frame (as Bevy's glTF loader does by default)
    #[default]
    Dynamic,
    /// Bounds of the bind pose, fixed; enough when the pose stays close to it
    BindPose,
    /// Never culled
    NoCulling,
}

/// Choices for every spawn function and conversion of this crate. [`crate::RewriteBridgePlugin`] keeps one
/// as a resource, which [`crate::Bridge`] uses.
#[derive(Resource, Clone, Debug)]
pub struct SpawnOptions {
    /// Build a box-filtered mip chain for every image, so textures do not shimmer in the distance.
    /// Bridge images carry one level only. sRGB images are averaged in linear light.
    pub mipmaps: bool,
    /// Anisotropic filtering, 1 (off) to 16
    pub anisotropy: u16,
    /// Multiplies every material's emissive colour: Bevy's emissive is
    /// `emissive_factor * emissive_strength * emissive_scale` (times the emissive image). Bridge
    /// emissive factors are relative, and 1.0 is reference white. Bevy's emissive is in cd/m², but with
    /// the default [`SpawnOptions::emissive_exposure_weight`] of 0 the camera's exposure does not touch
    /// it, so 1.0 stays white under any exposure and this scale stays at 1.0, as in Bevy's own glTF loader.
    /// Raise it to push colours into bloom.
    pub emissive_scale: f32,
    /// Bevy's `StandardMaterial::emissive_exposure_weight`: 0 (the default) leaves emissive colours
    /// unaffected by the camera's exposure; 1 multiplies them by it like every light. With 1, set
    /// `emissive_scale` to [`white_luminance`] of your camera's exposure for a factor of 1.0 to read as
    /// white.
    pub emissive_exposure_weight: f32,
    /// cd/m² per 1.0 of the scene's [`rb::Environment`] (sky colour or picture, which are relative like
    /// emissive factors). Unlike emissive colours the sky is lit through the camera's exposure, so
    /// this is the number that makes the sky read right under yours: [`white_luminance`] of the camera's
    /// `Exposure`. The default is that of Bevy's default exposure (about 995); set it if your camera
    /// uses another.
    pub environment_scale: f32,
    /// Whether scene lights cast shadows
    pub light_shadows: bool,
    /// Range given to point and spot lights whose range is `None` ("no limit"): Bevy needs a number,
    /// and inverse-square falloff makes the light negligible long before it
    pub unbounded_light_range: f32,
    /// Largest side of the cube map made from an equirectangular sky (rounded down to a power of two)
    pub sky_face_size: u32,
    /// Whether an equirectangular sky also lights the scene (a filtered environment map on the camera)
    pub sky_lighting: bool,
    pub skinned_bounds: SkinnedBounds,
    /// Where geometry and images live after upload; see the module docs
    pub usage: RenderAssetUsages,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            mipmaps: true,
            anisotropy: 8,
            emissive_scale: 1.0,
            emissive_exposure_weight: 0.0,
            environment_scale: white_luminance(Exposure::default()),
            light_shadows: false,
            unbounded_light_range: 1000.0,
            sky_face_size: 1024,
            sky_lighting: true,
            skinned_bounds: SkinnedBounds::default(),
            usage: RenderAssetUsages::RENDER_WORLD,
        }
    }
}

/// The asset collections [`spawn_model`] adds to, as one system parameter
#[derive(SystemParam)]
pub struct BridgeAssets<'w> {
    pub meshes: ResMut<'w, Assets<Mesh>>,
    pub materials: ResMut<'w, Assets<StandardMaterial>>,
    pub images: ResMut<'w, Assets<Image>>,
    pub inverse_bindposes: ResMut<'w, Assets<SkinnedMeshInverseBindposes>>,
}

/// Marks the root entity of a spawned model
#[derive(Component, Debug, Clone)]
pub struct BridgeModel {
    pub name: String,
}

/// Marks the entity of a model's node, with its index in [`Model::nodes`](rb::Model::nodes)
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeNode(pub usize);

/// The entities and handles [`spawn_model`] created
#[derive(Debug, Clone)]
pub struct SpawnedModel {
    pub root: Entity,
    /// One per model node, in the model's order
    pub nodes: Vec<Entity>,
    /// One per model mesh, in the model's order
    pub meshes: Vec<Entity>,
    /// One per model material, in the model's order
    pub materials: Vec<Handle<StandardMaterial>>,
}

/// Index of the first node with this name
pub fn find_node(model: &rb::Model, name: &str) -> Option<usize> {
    model.nodes.iter().position(|n| n.name == name)
}

/// For every node, whether it is `node` or one of its descendants
pub fn subtree(model: &rb::Model, node: usize) -> Vec<bool> {
    let mut inside = vec![false; model.nodes.len()];
    for (i, n) in model.nodes.iter().enumerate() {
        // Parents come before their children, so one pass is enough
        inside[i] = i == node || n.parent.is_some_and(|p| inside[p]);
    }
    inside
}

/// A unit quaternion from a bridge rotation. A zero (or non-finite) quaternion, which `validate` rejects but a
/// hand-built model can still carry, would normalize to NaN and poison every matrix below it: it is the identity here.
fn unit_rotation(q: [f32; 4]) -> Quat {
    let quat = Quat::from_array(q);
    let len = quat.length();
    if len.is_finite() && len > 1e-6 {
        quat * (1.0 / len)
    } else {
        warn!("a rotation {q:?} cannot be normalized; using the identity");
        Quat::IDENTITY
    }
}

/// The local transform of a node
pub fn node_transform(node: &rb::Node) -> Transform {
    Transform {
        translation: Vec3::from(node.translation),
        rotation: unit_rotation(node.rotation),
        scale: Vec3::from(node.scale),
    }
}

/// Every node's rest transform in model space (relative to the model's root). A parent that does not come
/// before its child (a model `validate` rejects) is ignored: the node is a root.
pub fn rest_globals(model: &rb::Model) -> Vec<Mat4> {
    let mut out: Vec<Mat4> = Vec::with_capacity(model.nodes.len());
    for (i, n) in model.nodes.iter().enumerate() {
        let local = node_transform(n).to_matrix();
        let parent = match n.parent {
            Some(p) if p < i => Some(out[p]),
            Some(p) => {
                warn!(
                    "{}: node {i} ({}) has parent {p}, which does not come before it; treated as a root",
                    model.name, n.name
                );
                None
            }
            None => None,
        };
        out.push(parent.map_or(local, |g| g * local));
    }
    out
}

/// Where a mesh's vertices are in model space with every node at rest: rigid meshes go through their
/// node's transform, skinned meshes through their joints (as Bevy's skinning does).
///
/// Never panics on a model `validate` would reject (a node, joint or inverse-bind index out of range, joint
/// and weight arrays that do not match the positions): such a mesh is skipped with a warning and gives no
/// vertices.
pub fn rest_positions(model: &rb::Model, mesh: &rb::Mesh, globals: &[Mat4]) -> Vec<Vec3> {
    match mesh.skin.and_then(|s| model.skins.get(s)) {
        Some(skin) => {
            if skin.joints.len() != skin.inverse_bind.len()
                || mesh.joints.len() != mesh.positions.len()
                || mesh.weights.len() != mesh.positions.len()
            {
                warn!(
                    "{}: mesh {} has {} positions, {} joint sets, {} weight sets and a skin with {} joints and {} inverse binds; skipped",
                    model.name,
                    mesh.name,
                    mesh.positions.len(),
                    mesh.joints.len(),
                    mesh.weights.len(),
                    skin.joints.len(),
                    skin.inverse_bind.len()
                );
                return Vec::new();
            }
            let mut joint: Vec<Mat4> = Vec::with_capacity(skin.joints.len());
            for (&node, ib) in skin.joints.iter().zip(&skin.inverse_bind) {
                let Some(g) = globals.get(node) else {
                    warn!(
                        "{}: a skin joint is node {node}, past the model's {} nodes; mesh {} skipped",
                        model.name,
                        globals.len(),
                        mesh.name
                    );
                    return Vec::new();
                };
                joint.push(*g * Mat4::from_cols_array(ib));
            }
            let mut out = Vec::with_capacity(mesh.positions.len());
            for (&p, (j, w)) in mesh
                .positions
                .iter()
                .zip(mesh.joints.iter().zip(&mesh.weights))
            {
                let p = Vec3::from(p);
                let mut sum = Vec3::ZERO;
                for k in 0..4 {
                    if w[k] > 0.0 {
                        let Some(m) = joint.get(usize::from(j[k])) else {
                            warn!(
                                "{}: mesh {} binds a vertex to joint {}, but the skin has {}; skipped",
                                model.name,
                                mesh.name,
                                j[k],
                                joint.len()
                            );
                            return Vec::new();
                        };
                        sum += w[k] * m.transform_point3(p);
                    }
                }
                out.push(sum);
            }
            out
        }
        None => {
            let Some(m) = globals.get(mesh.node) else {
                warn!(
                    "{}: mesh {} sits on node {}, past the model's {} nodes; skipped",
                    model.name,
                    mesh.name,
                    mesh.node,
                    globals.len()
                );
                return Vec::new();
            };
            mesh.positions
                .iter()
                .map(|&p| m.transform_point3(Vec3::from(p)))
                .collect()
        }
    }
}

/// Model-space bounds of the meshes for which `keep` is true, at rest; `None` without vertices
pub fn rest_bounds_where(
    model: &rb::Model,
    keep: impl Fn(usize, &rb::Mesh) -> bool,
) -> Option<(Vec3, Vec3)> {
    let globals = rest_globals(model);
    let mut bounds: Option<(Vec3, Vec3)> = None;
    for (i, mesh) in model.meshes.iter().enumerate() {
        if !keep(i, mesh) {
            continue;
        }
        for p in rest_positions(model, mesh, &globals) {
            bounds = Some(bounds.map_or((p, p), |(lo, hi)| (lo.min(p), hi.max(p))));
        }
    }
    bounds
}

/// Model-space bounds of the whole model at rest (unlike [`rb::Model::bounds`], which leaves every
/// mesh in its own space)
pub fn rest_bounds(model: &rb::Model) -> Option<(Vec3, Vec3)> {
    rest_bounds_where(model, |_, _| true)
}

/// A bridge mesh as a Bevy triangle-list mesh. Missing normals are computed (smooth); missing tangents
/// are generated with MikkTSpace when `tangents_needed` (a normal-mapped material) and the mesh has
/// normals and UVs. Skinned meshes get joint indices and weights.
pub fn to_bevy_mesh(mesh: &rb::Mesh, tangents_needed: bool, usage: RenderAssetUsages) -> Mesh {
    into_bevy_mesh(mesh.clone(), tangents_needed, usage)
}

/// [`to_bevy_mesh`], moving the vertex data instead of copying it
pub fn into_bevy_mesh(mesh: rb::Mesh, tangents_needed: bool, usage: RenderAssetUsages) -> Mesh {
    let has_triangles = !mesh.indices.is_empty();
    let has_uvs = !mesh.uvs.is_empty();
    let indices = if mesh.positions.len() <= usize::from(u16::MAX) {
        Indices::U16(mesh.indices.iter().map(|&i| i as u16).collect())
    } else {
        Indices::U32(mesh.indices)
    };
    let mut out = Mesh::new(PrimitiveTopology::TriangleList, usage)
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, mesh.positions);
    out.insert_indices(indices);
    if mesh.normals.is_empty() {
        if has_triangles {
            out.compute_smooth_normals();
        }
    } else {
        out.insert_attribute(Mesh::ATTRIBUTE_NORMAL, mesh.normals);
    }
    if has_uvs {
        out.insert_attribute(Mesh::ATTRIBUTE_UV_0, mesh.uvs);
    }
    if !mesh.tangents.is_empty() {
        out.insert_attribute(Mesh::ATTRIBUTE_TANGENT, mesh.tangents);
    } else if tangents_needed && has_uvs && has_triangles {
        if let Err(e) = out.generate_tangents() {
            warn!("{}: no tangents: {e}", mesh.name);
        }
    }
    if mesh.skin.is_some() {
        out.insert_attribute(
            Mesh::ATTRIBUTE_JOINT_INDEX,
            VertexAttributeValues::Uint16x4(mesh.joints),
        );
        out.insert_attribute(Mesh::ATTRIBUTE_JOINT_WEIGHT, mesh.weights);
    }
    out
}

/// sRGB byte to linear light
fn srgb_to_linear(v: u8) -> f32 {
    let c = f32::from(v) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let s = if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round() as u8
}

/// Every mip level of an RGBA8 image after the first, each a 2x2 box filter of the one above (a
/// level with an odd size repeats its last row or column). Colour channels of sRGB images are
/// averaged in linear light, alpha always linearly.
pub fn mip_chain(width: u32, height: u32, rgba: &[u8], srgb: bool) -> Vec<Vec<u8>> {
    // An empty image has no levels, and one whose pixels are fewer than its size says would index past the end
    // (both are images `validate` rejects, but this is a public function)
    let needed = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4));
    if width == 0 || height == 0 || needed.is_none_or(|n| rgba.len() < n) {
        warn!(
            "mip_chain: a {width}x{height} image with {} bytes has no mip levels",
            rgba.len()
        );
        return Vec::new();
    }
    let decode: Vec<f32> = (0..=255u8)
        .map(|v| {
            if srgb {
                srgb_to_linear(v)
            } else {
                f32::from(v) / 255.0
            }
        })
        .collect();
    let encode = |c: f32| {
        if srgb {
            linear_to_srgb(c)
        } else {
            (c.clamp(0.0, 1.0) * 255.0).round() as u8
        }
    };
    let (mut w, mut h) = (width as usize, height as usize);
    let mut levels: Vec<Vec<u8>> = Vec::new();
    while w > 1 || h > 1 {
        let prev: &[u8] = levels.last().map_or(rgba, |l| l.as_slice());
        let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
        let mut next = Vec::with_capacity(nw * nh * 4);
        for y in 0..nh {
            let (y0, y1) = ((2 * y).min(h - 1), (2 * y + 1).min(h - 1));
            for x in 0..nw {
                let (x0, x1) = ((2 * x).min(w - 1), (2 * x + 1).min(w - 1));
                let px = [(x0, y0), (x1, y0), (x0, y1), (x1, y1)];
                for k in 0..4 {
                    let sum: f32 = px
                        .iter()
                        .map(|&(sx, sy)| {
                            let v = prev[(sy * w + sx) * 4 + k];
                            if k == 3 {
                                f32::from(v) / 255.0
                            } else {
                                decode[usize::from(v)]
                            }
                        })
                        .sum();
                    let avg = sum / 4.0;
                    next.push(if k == 3 {
                        (avg * 255.0).round() as u8
                    } else {
                        encode(avg)
                    });
                }
            }
        }
        levels.push(next);
        (w, h) = (nw, nh);
    }
    levels
}

/// A bridge image as a repeating, linearly filtered Bevy texture: sRGB for colour and emissive, linear
/// for the rest
pub fn to_bevy_image(img: &rb::Image, srgb: bool, options: &SpawnOptions) -> Image {
    into_bevy_image(img.clone(), srgb, options)
}

/// [`to_bevy_image`], moving the pixels instead of copying them
pub fn into_bevy_image(img: rb::Image, srgb: bool, options: &SpawnOptions) -> Image {
    let format = if srgb {
        TextureFormat::Rgba8UnormSrgb
    } else {
        TextureFormat::Rgba8Unorm
    };
    let size = Extent3d {
        width: img.width,
        height: img.height,
        depth_or_array_layers: 1,
    };
    let mut mip_level_count = 1;
    let mut data = img.rgba;
    if options.mipmaps && (img.width > 1 || img.height > 1) {
        let levels = mip_chain(img.width, img.height, &data, srgb);
        mip_level_count += levels.len() as u32;
        data.reserve_exact(levels.iter().map(Vec::len).sum());
        for l in levels {
            data.extend_from_slice(&l);
        }
    }
    // The size check in Image::new only knows about the first level, so set the chain afterwards
    let mut image = Image::new_uninit(size, TextureDimension::D2, format, options.usage);
    image.texture_descriptor.mip_level_count = mip_level_count;
    image.data = Some(data);
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        // Models commonly tile UVs beyond 0..1
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        mipmap_filter: ImageFilterMode::Linear,
        anisotropy_clamp: options.anisotropy.clamp(1, 16),
        ..ImageSamplerDescriptor::linear()
    });
    image
}

fn alpha_mode(a: rb::AlphaMode) -> AlphaMode {
    match a {
        rb::AlphaMode::Opaque => AlphaMode::Opaque,
        rb::AlphaMode::Mask(cutoff) => AlphaMode::Mask(cutoff),
        rb::AlphaMode::Blend => AlphaMode::Blend,
    }
}

/// A bridge material as a [`StandardMaterial`], adding its images to `images`
pub fn to_standard_material(
    m: &rb::Material,
    images: &mut Assets<Image>,
    options: &SpawnOptions,
) -> StandardMaterial {
    into_standard_material(m.clone(), images, options)
}

/// [`to_standard_material`], moving the images instead of copying them
pub fn into_standard_material(
    m: rb::Material,
    images: &mut Assets<Image>,
    options: &SpawnOptions,
) -> StandardMaterial {
    let mut add = |img: Option<rb::Image>, srgb: bool| {
        img.map(|i| images.add(into_bevy_image(i, srgb, options)))
    };
    let [r, g, b, a] = m.base_color_factor;
    let [er, eg, eb] = m.emissive_factor;
    StandardMaterial {
        base_color: Color::linear_rgba(r, g, b, a),
        base_color_texture: add(m.base_color, true),
        metallic: m.metallic,
        perceptual_roughness: m.roughness,
        metallic_roughness_texture: add(m.metallic_roughness, false),
        normal_map_texture: add(m.normal, false),
        // Scaling a LinearRgba would scale its alpha too
        emissive: LinearRgba::rgb(
            er * options.emissive_scale,
            eg * options.emissive_scale,
            eb * options.emissive_scale,
        ),
        emissive_exposure_weight: options.emissive_exposure_weight,
        emissive_texture: add(m.emissive, true),
        alpha_mode: alpha_mode(m.alpha),
        double_sided: m.double_sided,
        cull_mode: if m.double_sided {
            None
        } else {
            Some(Face::Back)
        },
        ..default()
    }
}

/// Spawns a model under a new root entity at the origin; see the module docs for the layout. Move the
/// model with the root's [`Transform`], or parent the root to another entity.
pub fn spawn_model(
    commands: &mut Commands,
    assets: &mut BridgeAssets,
    model: &rb::Model,
    options: &SpawnOptions,
) -> SpawnedModel {
    spawn_model_owned(commands, assets, model.clone(), options)
}

/// [`spawn_model`], moving the model's pixels and vertices into Bevy's assets instead of copying them,
/// which keeps the peak memory of spawning large models down
pub fn spawn_model_owned(
    commands: &mut Commands,
    assets: &mut BridgeAssets,
    mut model: rb::Model,
    options: &SpawnOptions,
) -> SpawnedModel {
    let root = commands
        .spawn((
            Name::new(model.name.clone()),
            BridgeModel {
                name: model.name.clone(),
            },
            Transform::default(),
            Visibility::default(),
        ))
        .id();

    let mut nodes: Vec<Entity> = Vec::with_capacity(model.nodes.len());
    for (i, n) in model.nodes.iter().enumerate() {
        // Parents come first (`validate` checks it); a parent that does not is ignored rather than indexed
        let parent = n.parent.and_then(|p| nodes.get(p).copied()).unwrap_or(root);
        nodes.push(
            commands
                .spawn((
                    Name::new(n.name.clone()),
                    BridgeNode(i),
                    node_transform(n),
                    Visibility::default(),
                    ChildOf(parent),
                ))
                .id(),
        );
    }

    let normal_mapped: Vec<bool> = model.materials.iter().map(|m| m.normal.is_some()).collect();
    let materials: Vec<Handle<StandardMaterial>> = std::mem::take(&mut model.materials)
        .into_iter()
        .map(|m| {
            assets
                .materials
                .add(into_standard_material(m, &mut assets.images, options))
        })
        .collect();

    // Rigid meshes whose node rests mirrored need culling flipped, like mirrored scene instances. Poses
    // that mirror later (a negative scale animated into a node) are not followed.
    let globals = rest_globals(&model);
    let mut mirrored: Vec<Option<Handle<StandardMaterial>>> = vec![None; materials.len()];

    // A skin whose joints do not name nodes of the model, or that has no inverse bind for a joint, cannot be
    // built; it is `None` and the meshes bound to it are skipped (`validate` rejects such models)
    let skins: Vec<Option<SkinnedMesh>> = model
        .skins
        .iter()
        .map(|s| {
            if s.joints.len() > MAX_JOINTS {
                warn!(
                    "{}: a skin with {} joints is past Bevy's {MAX_JOINTS}",
                    model.name,
                    s.joints.len()
                );
            }
            if s.joints.len() != s.inverse_bind.len() || s.joints.iter().any(|&j| j >= nodes.len())
            {
                warn!(
                    "{}: a skin with {} joints and {} inverse binds does not fit the model's {} nodes; its meshes are skipped",
                    model.name,
                    s.joints.len(),
                    s.inverse_bind.len(),
                    nodes.len()
                );
                return None;
            }
            let ibm: Vec<Mat4> = s.inverse_bind.iter().map(Mat4::from_cols_array).collect();
            Some(SkinnedMesh {
                inverse_bindposes: assets
                    .inverse_bindposes
                    .add(SkinnedMeshInverseBindposes::from(ibm)),
                joints: s.joints.iter().map(|&j| nodes[j]).collect(),
            })
        })
        .collect();

    let mut meshes = Vec::with_capacity(model.meshes.len());
    for mesh in std::mem::take(&mut model.meshes) {
        let tangents_needed = normal_mapped.get(mesh.material).copied().unwrap_or(false);
        let (name, node, material) = (mesh.name.clone(), mesh.node, mesh.material);
        let skin = mesh
            .skin
            .and_then(|s| skins.get(s))
            .and_then(Option::as_ref);
        // A mesh that names a node, a material or a skin the model does not have cannot be drawn. Skip it
        // with a warning, but keep an (empty) entity so `SpawnedModel::meshes` stays one per model mesh.
        if node >= nodes.len()
            || material >= materials.len()
            || (mesh.skin.is_some() && skin.is_none())
        {
            warn!(
                "{}: mesh {name} uses node {node}, material {material} and skin {:?}, which the model ({} nodes, {} materials, {} skins) does not have; skipped",
                model.name,
                mesh.skin,
                nodes.len(),
                materials.len(),
                skins.len()
            );
            meshes.push(commands.spawn((Name::new(name), ChildOf(root))).id());
            continue;
        }
        let mut bevy_mesh = into_bevy_mesh(mesh, tangents_needed, options.usage);
        if skin.is_some() && options.skinned_bounds == SkinnedBounds::Dynamic {
            if let Err(e) = bevy_mesh.generate_skinned_mesh_bounds() {
                warn!("{name}: no skinned bounds: {e}");
            }
        }
        // Skinned vertices follow their joints wherever the mesh entity is; keep it at the root, where
        // the bind-pose bounds Bevy computes for it line up with the joints at rest
        let parent = if skin.is_some() { root } else { nodes[node] };
        let material_handle =
            if skin.is_none() && Mat3::from_mat4(globals[node]).determinant() < 0.0 {
                mirrored[material]
                    .get_or_insert_with(|| {
                        mirrored_material(&mut assets.materials, &materials[material])
                    })
                    .clone()
            } else {
                materials[material].clone()
            };
        let mut e = commands.spawn((
            Name::new(name),
            Mesh3d(assets.meshes.add(bevy_mesh)),
            MeshMaterial3d(material_handle),
            ChildOf(parent),
        ));
        if let Some(s) = skin {
            e.insert(s.clone());
            match options.skinned_bounds {
                SkinnedBounds::Dynamic => {
                    e.insert(DynamicSkinnedMeshBounds);
                }
                SkinnedBounds::BindPose => {}
                SkinnedBounds::NoCulling => {
                    e.insert(NoFrustumCulling);
                }
            }
        }
        meshes.push(e.id());
    }

    SpawnedModel {
        root,
        nodes,
        meshes,
        materials,
    }
}

/// Whether the affine matrix `m` shears: its linear part is not a rotation times a scale (its columns are not
/// pairwise perpendicular), so no [`Transform`] can hold it. A parent scaled `[2, 1, 1]` over a child turned 45
/// degrees is the usual way to get one.
fn has_shear(m: &Mat4) -> bool {
    let (a, b, c) = (
        m.x_axis.truncate(),
        m.y_axis.truncate(),
        m.z_axis.truncate(),
    );
    let skew =
        |u: Vec3, v: Vec3| u.dot(v).abs() > 1e-4 * u.length() * v.length().max(f32::MIN_POSITIVE);
    skew(a, b) || skew(a, c) || skew(b, c)
}

/// Puts the matrix `m` into the mesh's vertices: positions go through it, normals through its inverse transpose,
/// tangents through its linear part. A matrix that mirrors (negative determinant) also turns the triangles round,
/// and the tangents' handedness over, so the mesh needs no mirrored material. Returns false, leaving the mesh as it
/// was, when `m` cannot be inverted (a zero scale).
fn bake_matrix(mesh: &mut rb::Mesh, m: Mat4) -> bool {
    let lin = Mat3::from_mat4(m);
    let det = lin.determinant();
    if !det.is_finite() || det.abs() < 1e-12 {
        return false;
    }
    let normal = lin.inverse().transpose();
    for p in &mut mesh.positions {
        *p = m.transform_point3(Vec3::from(*p)).to_array();
    }
    for n in &mut mesh.normals {
        *n = (normal * Vec3::from(*n)).normalize_or_zero().to_array();
    }
    let handedness = if det < 0.0 { -1.0 } else { 1.0 };
    for t in &mut mesh.tangents {
        let v = (lin * Vec3::new(t[0], t[1], t[2])).normalize_or_zero();
        *t = [v.x, v.y, v.z, t[3] * handedness];
    }
    if det < 0.0 {
        let (triangles, _) = mesh.indices.as_chunks_mut::<3>();
        for tri in triangles {
            tri.swap(1, 2);
        }
    }
    true
}

/// One mesh of a static model, uploaded once, to be placed any number of times
#[derive(Clone, Debug)]
pub struct MeshPart {
    pub mesh: Handle<Mesh>,
    pub material: Handle<StandardMaterial>,
    /// The mesh's node at rest, in the model's space (the identity for single-node models, and for a node
    /// whose matrix shears, which [`prepare_parts`] bakes into the mesh because a `Transform` cannot hold it)
    pub transform: Transform,
}

/// Uploads a static model's meshes and materials once and returns one [`MeshPart`] per mesh, for scenes
/// that place a model many times: spawn one entity per placement and part with clones of the handles
/// (Bevy draws entities that share a mesh and a material in one instanced batch). The model's pixels and
/// vertices are moved into Bevy. Skinned meshes are left out (a scene model has no skeleton to pose).
pub fn prepare_parts(
    assets: &mut BridgeAssets,
    mut model: rb::Model,
    options: &SpawnOptions,
) -> Vec<MeshPart> {
    let globals = rest_globals(&model);
    let normal_mapped: Vec<bool> = model.materials.iter().map(|m| m.normal.is_some()).collect();
    let materials: Vec<Handle<StandardMaterial>> = std::mem::take(&mut model.materials)
        .into_iter()
        .map(|m| {
            assets
                .materials
                .add(into_standard_material(m, &mut assets.images, options))
        })
        .collect();
    let mut parts = Vec::with_capacity(model.meshes.len());
    for mesh in std::mem::take(&mut model.meshes) {
        if mesh.skin.is_some() {
            warn!("{}: a skinned mesh in a scene model is left out", mesh.name);
            continue;
        }
        let (node, material) = (mesh.node, mesh.material);
        let (Some(&at), Some(material_handle)) = (globals.get(node), materials.get(material))
        else {
            warn!(
                "{}: mesh {} uses node {node} and material {material}, which the model ({} nodes, {} materials) does not have; skipped",
                model.name,
                mesh.name,
                globals.len(),
                materials.len()
            );
            continue;
        };
        let tangents_needed = normal_mapped.get(material).copied().unwrap_or(false);
        let mut mesh = mesh;
        let transform = if at.abs_diff_eq(Mat4::IDENTITY, 1e-6) {
            Transform::IDENTITY
        } else if has_shear(&at) && bake_matrix(&mut mesh, at) {
            // A `Transform` is translation, rotation and scale: a node under a non-uniformly scaled parent and
            // turned itself is not (the matrix shears), and `Transform::from_matrix` would quietly draw
            // another shape. `spawn_model` keeps the node hierarchy, whose global transforms are full matrices;
            // here the matrix goes into the vertices instead, so both draw the same.
            Transform::IDENTITY
        } else {
            Transform::from_matrix(at)
        };
        let bevy_mesh = into_bevy_mesh(mesh, tangents_needed, options.usage);
        parts.push(MeshPart {
            mesh: assets.meshes.add(bevy_mesh),
            material: material_handle.clone(),
            transform,
        });
    }
    parts
}

/// Whether a transform with this scale mirrors what it moves (an odd number of negative components)
pub fn is_mirrored(scale: Vec3) -> bool {
    scale.is_negative_bitmask().count_ones() % 2 == 1
}

/// A copy of `material` for meshes under a mirroring transform (see [`is_mirrored`]), with back-face
/// culling turned into front-face culling.
///
/// A mirror flips the winding of the triangles on screen, and Bevy's culling does not account for it:
/// without this the visible faces of a mirrored mesh are the ones culled, and you look into the model.
/// glTF asks for the flipped winding too (its spec says a negative determinant inverts it), and Bevy's
/// own glTF loader does the same with a material copy. Double-sided materials do not cull and are
/// copied unchanged. The copy shares the original's images.
pub fn mirrored_material(
    materials: &mut Assets<StandardMaterial>,
    material: &Handle<StandardMaterial>,
) -> Handle<StandardMaterial> {
    let Some(original) = materials.get(material) else {
        return material.clone();
    };
    let mut copy = original.clone();
    if copy.cull_mode == Some(Face::Back) {
        copy.cull_mode = Some(Face::Front);
    }
    materials.add(copy)
}

/// A bridge instance transform as a Bevy one. A negative scale mirrors, which needs
/// [`mirrored_material`] on the meshes it moves ([`crate::spawn_scene`] does that).
pub fn instance_transform(t: &rb::Transform) -> Transform {
    Transform {
        translation: Vec3::from(t.translation),
        rotation: unit_rotation(t.rotation),
        scale: Vec3::from(t.scale),
    }
}

/// The transform of a part placed by an instance. Parts at the model's origin (the identity) take the
/// instance's transform unchanged, so mirrored placements keep their sign (and need [`mirrored_material`]).
pub fn place(instance: &Transform, part: &MeshPart) -> Transform {
    if part.transform == Transform::IDENTITY {
        *instance
    } else {
        Transform::from_matrix(instance.to_matrix() * part.transform.to_matrix())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;

    /// A two-node model: a rigid triangle on a turned, shifted child node and a skinned triangle bound
    /// to both nodes, with a textured, normal-mapped material and a plain blended one
    fn model() -> rb::Model {
        let mut root = rb::Node::new("root", None);
        root.translation = [0.0, 1.0, 0.0];
        let mut arm = rb::Node::new("arm", Some(0));
        arm.translation = [2.0, 0.0, 0.0];
        // A quarter turn about +Y: +X goes to -Z
        let h = std::f32::consts::FRAC_1_SQRT_2;
        arm.rotation = [0.0, h, 0.0, h];
        let arm_rotation = Quat::from_array(arm.rotation);
        let tri = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let image = |w: u32, h: u32, v: u8| rb::Image {
            width: w,
            height: h,
            rgba: vec![v; (w * h * 4) as usize],
        };
        rb::Model {
            name: "test".into(),
            nodes: vec![root, arm],
            meshes: vec![
                rb::Mesh {
                    name: "rigid".into(),
                    node: 1,
                    material: 0,
                    positions: tri.to_vec(),
                    normals: vec![[0.0, 0.0, 1.0]; 3],
                    uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                    indices: vec![0, 1, 2],
                    ..default()
                },
                rb::Mesh {
                    name: "skinned".into(),
                    material: 1,
                    positions: tri.to_vec(),
                    joints: vec![[0, 1, 0, 0]; 3],
                    weights: vec![[0.5, 0.5, 0.0, 0.0]; 3],
                    skin: Some(0),
                    indices: vec![0, 1, 2],
                    ..default()
                },
            ],
            materials: vec![
                rb::Material {
                    name: "painted".into(),
                    base_color: Some(image(4, 2, 200)),
                    base_color_factor: [0.5, 0.25, 1.0, 1.0],
                    metallic_roughness: Some(image(2, 2, 255)),
                    metallic: 0.3,
                    roughness: 0.7,
                    normal: Some(image(1, 1, 128)),
                    emissive_factor: [2.0, 0.0, 0.0],
                    alpha: rb::AlphaMode::Mask(0.4),
                    ..default()
                },
                rb::Material {
                    name: "glass".into(),
                    alpha: rb::AlphaMode::Blend,
                    double_sided: true,
                    ..default()
                },
            ],
            // Both joints bound where they rest, so the skinned mesh rests where its positions say
            skins: vec![rb::Skin {
                joints: vec![0, 1],
                inverse_bind: vec![
                    Mat4::from_translation(Vec3::new(0.0, -1.0, 0.0)).to_cols_array(),
                    Mat4::from_rotation_translation(arm_rotation, Vec3::new(2.0, 1.0, 0.0))
                        .inverse()
                        .to_cols_array(),
                ],
            }],
        }
    }

    fn close(a: Vec3, b: Vec3) -> bool {
        a.distance(b) < 1e-5
    }

    #[test]
    fn the_test_model_is_valid() {
        assert_eq!(model().validate(), Ok(()));
    }

    #[test]
    fn rest_transforms_compose_down_the_hierarchy() {
        let m = model();
        let g = rest_globals(&m);
        assert!(close(
            g[1].transform_point3(Vec3::ZERO),
            Vec3::new(2.0, 1.0, 0.0)
        ));
        // The arm's +X points along the root's -Z
        assert!(close(g[1].transform_vector3(Vec3::X), Vec3::NEG_Z));
        let rigid = rest_positions(&m, &m.meshes[0], &g);
        assert!(close(rigid[1], Vec3::new(2.0, 1.0, -1.0)));
        // At rest the skin cancels out: vertices sit where they are stored
        let skinned = rest_positions(&m, &m.meshes[1], &g);
        assert!(close(skinned[2], Vec3::new(0.0, 1.0, 0.0)));
        let (lo, hi) = rest_bounds(&m).unwrap();
        assert!(
            close(lo, Vec3::new(0.0, 0.0, -1.0)) && close(hi, Vec3::new(2.0, 2.0, 0.0)),
            "{lo} {hi}"
        );
        let (lo, _) = rest_bounds_where(&m, |i, _| i == 0).unwrap();
        assert!(close(lo, Vec3::new(2.0, 1.0, -1.0)));
        assert_eq!(subtree(&m, 1), [false, true]);
        assert_eq!(find_node(&m, "arm"), Some(1));
    }

    #[test]
    fn meshes_keep_their_attributes() {
        let m = model();
        let rigid = to_bevy_mesh(&m.meshes[0], true, RenderAssetUsages::default());
        assert_eq!(rigid.count_vertices(), 3);
        assert!(matches!(rigid.indices(), Some(Indices::U16(i)) if i == &[0, 1, 2]));
        assert!(rigid.attribute(Mesh::ATTRIBUTE_UV_0).is_some());
        // Normal-mapped without stored tangents: generated
        assert!(rigid.attribute(Mesh::ATTRIBUTE_TANGENT).is_some());
        assert!(
            to_bevy_mesh(&m.meshes[0], false, RenderAssetUsages::default())
                .attribute(Mesh::ATTRIBUTE_TANGENT)
                .is_none()
        );

        let skinned = to_bevy_mesh(&m.meshes[1], false, RenderAssetUsages::default());
        // No stored normals: computed, facing +Z for this counter-clockwise triangle
        match skinned.attribute(Mesh::ATTRIBUTE_NORMAL) {
            Some(VertexAttributeValues::Float32x3(n)) => assert!(close(Vec3::from(n[0]), Vec3::Z)),
            other => panic!("normals: {other:?}"),
        }
        assert!(
            matches!(skinned.attribute(Mesh::ATTRIBUTE_JOINT_INDEX), Some(VertexAttributeValues::Uint16x4(j)) if j[0] == [0, 1, 0, 0])
        );
        assert!(skinned.attribute(Mesh::ATTRIBUTE_JOINT_WEIGHT).is_some());
        assert!(skinned.attribute(Mesh::ATTRIBUTE_UV_0).is_none());
    }

    #[test]
    fn mip_chains_halve_down_to_one_pixel() {
        // 4x2: left half black, right half white
        let mut rgba = Vec::new();
        for _ in 0..2 {
            for x in 0..4 {
                let v = if x < 2 { 0 } else { 255 };
                rgba.extend_from_slice(&[v, v, v, v]);
            }
        }
        let linear = mip_chain(4, 2, &rgba, false);
        assert_eq!(linear.iter().map(Vec::len).collect::<Vec<_>>(), [2 * 4, 4]);
        assert_eq!(linear[0], [0, 0, 0, 0, 255, 255, 255, 255]);
        assert_eq!(linear[1], [128; 4]);
        // In sRGB, half of white's light is 188, not 128; alpha stays linear
        let srgb = mip_chain(4, 2, &rgba, true);
        assert_eq!(srgb[1], [188, 188, 188, 128]);

        let options = SpawnOptions::default();
        let img = to_bevy_image(
            &rb::Image {
                width: 4,
                height: 2,
                rgba,
            },
            true,
            &options,
        );
        assert_eq!(img.texture_descriptor.mip_level_count, 3);
        assert_eq!(img.texture_descriptor.format, TextureFormat::Rgba8UnormSrgb);
        assert_eq!(img.data.as_ref().unwrap().len(), (8 + 2 + 1) * 4);
    }

    #[test]
    fn materials_map_every_field() {
        let m = model();
        let mut images = Assets::<Image>::default();
        let options = SpawnOptions {
            emissive_scale: 2.0,
            ..default()
        };
        let painted = to_standard_material(&m.materials[0], &mut images, &options);
        assert_eq!(painted.base_color, Color::linear_rgba(0.5, 0.25, 1.0, 1.0));
        assert_eq!((painted.metallic, painted.perceptual_roughness), (0.3, 0.7));
        assert_eq!(painted.emissive, LinearRgba::rgb(4.0, 0.0, 0.0));
        assert_eq!(painted.alpha_mode, AlphaMode::Mask(0.4));
        assert_eq!(
            (painted.double_sided, painted.cull_mode),
            (false, Some(Face::Back))
        );
        let format = |h: &Option<Handle<Image>>| {
            images
                .get(h.as_ref().unwrap())
                .unwrap()
                .texture_descriptor
                .format
        };
        assert_eq!(
            format(&painted.base_color_texture),
            TextureFormat::Rgba8UnormSrgb
        );
        assert_eq!(
            format(&painted.metallic_roughness_texture),
            TextureFormat::Rgba8Unorm
        );
        assert_eq!(
            format(&painted.normal_map_texture),
            TextureFormat::Rgba8Unorm
        );
        assert!(painted.emissive_texture.is_none());
        assert_eq!(images.len(), 3);

        let glass = to_standard_material(&m.materials[1], &mut images, &options);
        assert_eq!(glass.alpha_mode, AlphaMode::Blend);
        assert_eq!((glass.double_sided, glass.cull_mode), (true, None));
    }

    #[test]
    fn parts_share_handles_and_placements_keep_mirroring() {
        let mut world = World::new();
        world.init_resource::<Assets<Mesh>>();
        world.init_resource::<Assets<StandardMaterial>>();
        world.init_resource::<Assets<Image>>();
        world.init_resource::<Assets<SkinnedMeshInverseBindposes>>();
        let mut m = model();
        // Only the rigid mesh: scene models have no skeletons
        m.meshes.truncate(1);
        m.skins.clear();
        let parts = world
            .run_system_once(move |mut assets: BridgeAssets| {
                prepare_parts(&mut assets, m.clone(), &SpawnOptions::default())
            })
            .unwrap();
        assert_eq!(parts.len(), 1);
        // The mesh hangs under the turned, shifted arm node: its rest transform is kept in the part
        assert!(close(
            parts[0].transform.translation,
            Vec3::new(2.0, 1.0, 0.0)
        ));
        let at = instance_transform(&rb::Transform {
            translation: [10.0, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [-1.0, 1.0, 1.0],
        });
        assert_eq!(at.scale, Vec3::new(-1.0, 1.0, 1.0));
        let placed = place(&at, &parts[0]);
        // The point at the part's origin lands where the instance's matrix puts the node's origin
        let expected = at.to_matrix().transform_point3(Vec3::new(2.0, 1.0, 0.0));
        assert!(close(
            placed.to_matrix().transform_point3(Vec3::ZERO),
            expected
        ));
        // A part at the model's origin takes the instance's transform as it is
        let plain = MeshPart {
            transform: Transform::IDENTITY,
            ..parts[0].clone()
        };
        assert_eq!(place(&at, &plain), at);
    }

    #[test]
    fn spawning_builds_the_hierarchy_and_skin() {
        let mut world = World::new();
        world.init_resource::<Assets<Mesh>>();
        world.init_resource::<Assets<StandardMaterial>>();
        world.init_resource::<Assets<Image>>();
        world.init_resource::<Assets<SkinnedMeshInverseBindposes>>();
        let spawned = world
            .run_system_once(|mut commands: Commands, mut assets: BridgeAssets| {
                spawn_model(
                    &mut commands,
                    &mut assets,
                    &model(),
                    &SpawnOptions::default(),
                )
            })
            .unwrap();

        let parent = |e: Entity| world.get::<ChildOf>(e).map(|c| c.parent());
        assert_eq!(world.get::<BridgeModel>(spawned.root).unwrap().name, "test");
        assert_eq!(parent(spawned.root), None);
        assert_eq!(parent(spawned.nodes[0]), Some(spawned.root));
        assert_eq!(parent(spawned.nodes[1]), Some(spawned.nodes[0]));
        assert_eq!(
            world.get::<BridgeNode>(spawned.nodes[1]),
            Some(&BridgeNode(1))
        );
        assert_eq!(world.get::<Name>(spawned.nodes[1]).unwrap().as_str(), "arm");
        assert_eq!(
            world
                .get::<Transform>(spawned.nodes[1])
                .unwrap()
                .translation,
            Vec3::new(2.0, 0.0, 0.0)
        );

        // The rigid mesh hangs under its node, the skinned one under the root
        assert_eq!(parent(spawned.meshes[0]), Some(spawned.nodes[1]));
        assert_eq!(parent(spawned.meshes[1]), Some(spawned.root));
        assert!(world.get::<SkinnedMesh>(spawned.meshes[0]).is_none());
        let skin = world.get::<SkinnedMesh>(spawned.meshes[1]).unwrap();
        assert_eq!(skin.joints, spawned.nodes);
        let ibm = world
            .resource::<Assets<SkinnedMeshInverseBindposes>>()
            .get(&skin.inverse_bindposes)
            .unwrap();
        assert!(close(
            ibm[0].transform_point3(Vec3::new(0.0, 1.0, 0.0)),
            Vec3::ZERO
        ));
        assert!(world
            .get::<DynamicSkinnedMeshBounds>(spawned.meshes[1])
            .is_some());

        // Materials are shared per bridge material
        assert_eq!(spawned.materials.len(), 2);
        assert_eq!(
            world
                .get::<MeshMaterial3d<StandardMaterial>>(spawned.meshes[1])
                .unwrap()
                .0,
            spawned.materials[1]
        );
        assert_eq!(world.resource::<Assets<Mesh>>().len(), 2);
    }

    #[test]
    fn mirrored_nodes_cull_the_other_way() {
        let mut world = World::new();
        world.init_resource::<Assets<Mesh>>();
        world.init_resource::<Assets<StandardMaterial>>();
        world.init_resource::<Assets<Image>>();
        world.init_resource::<Assets<SkinnedMeshInverseBindposes>>();
        let mut m = model();
        m.meshes.truncate(1);
        m.skins.clear();
        // The arm node, and so the rigid mesh on it, is mirrored along x
        m.nodes[1].scale = [-1.0, 1.0, 1.0];
        let spawned = world
            .run_system_once(move |mut commands: Commands, mut assets: BridgeAssets| {
                spawn_model(&mut commands, &mut assets, &m, &SpawnOptions::default())
            })
            .unwrap();
        let used = world
            .get::<MeshMaterial3d<StandardMaterial>>(spawned.meshes[0])
            .unwrap()
            .0
            .clone();
        let materials = world.resource::<Assets<StandardMaterial>>();
        // The model's own material stays as it was; the mesh got a copy that culls front faces
        assert_eq!(
            materials.get(&spawned.materials[0]).unwrap().cull_mode,
            Some(Face::Back)
        );
        assert_ne!(used, spawned.materials[0]);
        assert_eq!(materials.get(&used).unwrap().cull_mode, Some(Face::Front));
        assert!(
            is_mirrored(Vec3::new(-1.0, 1.0, 1.0))
                && !is_mirrored(Vec3::new(-1.0, -1.0, 1.0))
                && is_mirrored(Vec3::NEG_ONE)
        );
    }

    // ----------------------------------------------------------------- review fixes (H2, M13)

    /// A parent scaled [2, 1, 1] over a child turned 45 degrees about +Z: the child's matrix shears
    fn sheared_model() -> rb::Model {
        let mut parent = rb::Node::new("parent", None);
        parent.scale = [2.0, 1.0, 1.0];
        let mut child = rb::Node::new("child", Some(0));
        child.translation = [1.0, 0.5, 0.0];
        let half = std::f32::consts::FRAC_PI_8; // 45 degrees is a half angle of 22.5
        child.rotation = [0.0, 0.0, half.sin(), half.cos()];
        rb::Model {
            name: "sheared".into(),
            nodes: vec![parent, child],
            meshes: vec![rb::Mesh {
                name: "m".into(),
                node: 1,
                positions: vec![
                    [0.0, 0.0, 0.0],
                    [1.0, 0.0, 0.0],
                    [0.0, 1.0, 0.0],
                    [1.0, 1.0, 0.5],
                ],
                normals: vec![[0.0, 0.0, 1.0]; 4],
                indices: vec![0, 1, 2, 1, 3, 2],
                ..default()
            }],
            materials: vec![rb::Material::default()],
            skins: vec![],
        }
    }

    fn mesh_world_points(app: &App, e: Entity) -> Vec<Vec3> {
        let world = app.world();
        let mesh = world
            .resource::<Assets<Mesh>>()
            .get(&world.get::<Mesh3d>(e).unwrap().0)
            .unwrap();
        let positions = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .and_then(|v| v.as_float3())
            .unwrap();
        let g = world.get::<GlobalTransform>(e).unwrap();
        positions
            .iter()
            .map(|&p| g.transform_point(Vec3::from(p)))
            .collect()
    }

    /// M13: a model drawn through `spawn_scene` (parts) and through `spawn_model` (the node hierarchy) must put
    /// every vertex in the same place. A parent scaled [2, 1, 1] over a child turned 45 degrees shears the child's
    /// matrix, which `Transform::from_matrix` (a flattened part transform) cannot hold.
    #[test]
    fn spawn_scene_and_spawn_model_place_a_sheared_node_alike() {
        use crate::scene::{spawn_scene, NoSource};
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            bevy::transform::TransformPlugin,
        ));
        app.init_asset::<Mesh>()
            .init_asset::<StandardMaterial>()
            .init_asset::<Image>()
            .init_asset::<SkinnedMeshInverseBindposes>();
        let model = sheared_model();
        assert!(
            has_shear(&rest_globals(&model)[1]),
            "the test needs a shearing matrix"
        );

        let spawned = app
            .world_mut()
            .run_system_once({
                let model = model.clone();
                move |mut commands: Commands, mut assets: BridgeAssets| {
                    spawn_model(&mut commands, &mut assets, &model, &SpawnOptions::default())
                }
            })
            .unwrap();
        let scene = rb::Scene {
            name: "s".into(),
            models: vec![rb::SceneModel::Inline(model.clone())],
            instances: vec![rb::Instance::new(
                0,
                rb::Transform::from_translation([0.0; 3]),
            )],
            ..default()
        };
        let scene_spawned = app
            .world_mut()
            .run_system_once(move |mut commands: Commands, mut assets: BridgeAssets| {
                spawn_scene(
                    &mut commands,
                    &mut assets,
                    scene.clone(),
                    &NoSource,
                    &SpawnOptions::default(),
                )
                .unwrap()
            })
            .unwrap();
        app.update();

        let via_model = mesh_world_points(&app, spawned.meshes[0]);
        let instance = scene_spawned.instances[0];
        let part_entity = if app.world().get::<Mesh3d>(instance).is_some() {
            instance
        } else {
            app.world().get::<Children>(instance).unwrap()[0]
        };
        let via_scene = mesh_world_points(&app, part_entity);
        assert_eq!(via_model.len(), via_scene.len());
        for (a, b) in via_model.iter().zip(&via_scene) {
            assert!(a.distance(*b) < 1e-5, "{a} against {b}");
        }
        // The expected positions are the matrix product, not what a flattened Transform draws
        let globals = rest_globals(&model);
        let expected: Vec<Vec3> = model.meshes[0]
            .positions
            .iter()
            .map(|&p| globals[1].transform_point3(Vec3::from(p)))
            .collect();
        for (a, b) in via_scene.iter().zip(&expected) {
            assert!(a.distance(*b) < 1e-5, "{a} against {b}");
        }
        let flattened = Transform::from_matrix(globals[1]);
        let off: f32 = model.meshes[0]
            .positions
            .iter()
            .zip(&expected)
            .map(|(&p, e)| flattened.transform_point(Vec3::from(p)).distance(*e))
            .fold(0.0, f32::max);
        assert!(
            off > 0.05,
            "a flattened Transform would be off by {off}: the test would not catch the bug"
        );
        // And a node without shear is still a part transform, not baked into the mesh
        let mut plain = sheared_model();
        plain.nodes[0].scale = [2.0, 2.0, 2.0];
        let parts = app
            .world_mut()
            .run_system_once(move |mut assets: BridgeAssets| {
                prepare_parts(&mut assets, plain.clone(), &SpawnOptions::default())
            })
            .unwrap();
        assert_ne!(parts[0].transform, Transform::IDENTITY);
    }

    #[test]
    fn baking_a_mirroring_matrix_turns_the_triangles_and_the_tangent_handedness() {
        let mut mesh = rb::Mesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            tangents: vec![[1.0, 0.0, 0.0, 1.0]; 3],
            indices: vec![0, 1, 2],
            ..default()
        };
        // mirror x, scale y by 3, and turn: a mirror with shear
        let m = Mat4::from_rotation_z(0.6) * Mat4::from_scale(Vec3::new(-2.0, 3.0, 1.0));
        assert!(bake_matrix(&mut mesh, m));
        assert_eq!(mesh.indices, vec![0, 2, 1]);
        assert_eq!(mesh.tangents[0][3], -1.0);
        let n = Vec3::from(mesh.normals[0]);
        assert!(
            (n.length() - 1.0).abs() < 1e-5 && n.dot(Vec3::Z) > 0.99,
            "{n}"
        );
        // a matrix that cannot be inverted is left alone
        let before = mesh.positions.clone();
        assert!(!bake_matrix(
            &mut mesh,
            Mat4::from_scale(Vec3::new(1.0, 0.0, 1.0))
        ));
        assert_eq!(mesh.positions, before);
    }

    /// H2: the consumers never panic on a model `validate` would reject
    #[test]
    fn consumers_skip_what_validate_would_reject() {
        // a joint node past the model
        let mut m = model();
        m.skins[0].joints[0] = 99;
        assert!(m.validate().is_err());
        let globals = rest_globals(&m);
        assert!(rest_positions(&m, &m.meshes[1], &globals).is_empty());
        let (lo, hi) = rest_bounds(&m).expect("the rigid mesh still has bounds");
        assert!(lo.is_finite() && hi.is_finite());

        // a vertex bound to a joint the skin does not have, joint and inverse-bind counts that differ, short weight arrays
        let mut m = model();
        m.meshes[1].joints[0] = [9, 0, 0, 0];
        m.meshes[1].weights[0] = [1.0, 0.0, 0.0, 0.0];
        assert!(rest_positions(&m, &m.meshes[1], &rest_globals(&m)).is_empty());
        let mut m = model();
        m.skins[0].inverse_bind.pop();
        assert!(rest_positions(&m, &m.meshes[1], &rest_globals(&m)).is_empty());
        let mut m = model();
        m.meshes[1].weights.pop();
        assert!(rest_positions(&m, &m.meshes[1], &rest_globals(&m)).is_empty());

        // a rigid mesh on a node that is not there
        let mut m = model();
        m.meshes[0].node = 99;
        assert!(rest_positions(&m, &m.meshes[0], &rest_globals(&m)).is_empty());
        assert!(rest_bounds(&m).is_some());

        // a zero (or NaN) rotation stays finite; a parent that does not come first is a root
        let mut m = model();
        m.nodes[1].rotation = [0.0; 4];
        assert_eq!(node_transform(&m.nodes[1]).rotation, Quat::IDENTITY);
        assert!(rest_globals(&m).iter().all(|g| g.is_finite()));
        m.nodes[1].rotation = [f32::NAN, 0.0, 0.0, 1.0];
        assert!(node_transform(&m.nodes[1]).rotation.is_finite());
        m.nodes[0].parent = Some(1);
        assert!(rest_globals(&m).iter().all(|g| g.is_finite()));
        let inst = instance_transform(&rb::Transform {
            rotation: [0.0; 4],
            ..rb::Transform::from_translation([1.0, 2.0, 3.0])
        });
        assert_eq!(inst.rotation, Quat::IDENTITY);

        // mip_chain on an empty image, and on pixels fewer than the size says
        assert!(mip_chain(0, 4, &[], true).is_empty());
        assert!(mip_chain(4, 0, &[], false).is_empty());
        assert!(mip_chain(0, 0, &[], true).is_empty());
        assert!(mip_chain(2, 2, &[0; 7], true).is_empty());
        assert!(mip_chain(u32::MAX, u32::MAX, &[0; 4], true).is_empty());
        let levels = mip_chain(2, 2, &[255; 16], true);
        assert_eq!(levels.len(), 1);
        assert_eq!(levels[0], vec![255; 4]);
    }

    #[test]
    fn spawning_a_model_with_dangling_indices_does_not_panic() {
        let mut world = World::new();
        world.init_resource::<Assets<Mesh>>();
        world.init_resource::<Assets<StandardMaterial>>();
        world.init_resource::<Assets<Image>>();
        world.init_resource::<Assets<SkinnedMeshInverseBindposes>>();
        let mut m = model();
        m.nodes[1].parent = Some(7); // a parent that is not there
        m.meshes[0].node = 99;
        m.meshes[1].material = 9;
        m.skins[0].joints[1] = 50;
        let n_meshes = m.meshes.len();
        let for_parts = m.clone();
        let spawned = world
            .run_system_once(move |mut commands: Commands, mut assets: BridgeAssets| {
                spawn_model(&mut commands, &mut assets, &m, &SpawnOptions::default())
            })
            .unwrap();
        // one entity per mesh, as ever, the broken ones empty
        assert_eq!(spawned.meshes.len(), n_meshes);
        assert!(world.get::<Mesh3d>(spawned.meshes[0]).is_none());
        assert!(world.get::<Mesh3d>(spawned.meshes[1]).is_none());
        let mut only_rigid = for_parts;
        only_rigid.meshes.truncate(1);
        only_rigid.skins.clear();
        let parts = world
            .run_system_once(move |mut assets: BridgeAssets| {
                prepare_parts(&mut assets, only_rigid.clone(), &SpawnOptions::default())
            })
            .unwrap();
        assert!(
            parts.is_empty(),
            "the mesh on a node that is not there is skipped"
        );
    }
}
