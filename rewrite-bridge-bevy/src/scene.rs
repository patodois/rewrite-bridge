//! Scenes: [`rb::Scene`] in a Bevy world, with instancing, physical lights and the sky.
//!
//! [`spawn_scene`] makes one root entity ([`BridgeScene`]) and, under it:
//!
//! - one entity per [`rb::Instance`] ([`BridgeInstance`]). Every placement of a scene model shares
//!   that model's [`Mesh`] and [`StandardMaterial`] handles, so a forest of ten thousand trees holds a
//!   few meshes and Bevy draws the trees in a few instanced batches. A model of one mesh at its origin
//!   puts the mesh components on the instance entity itself; any other shape puts one child per mesh
//!   under it. Mirrored placements (a negative scale) get a copy of the material with front-face
//!   culling ([`mirrored_material`]), made once per material and shared by every mirrored instance, so
//!   they show their outside as glTF says they should;
//! - one entity per [`rb::Light`], in physical units (below).
//!
//! Models are uploaded once per scene model, and not at all if no instance uses them. Inline models
//! are moved into Bevy; [`rb::SceneModel::Asset`] references are loaded through a [`ModelSource`] you
//! provide, once per distinct reference even when several scene models name the same asset. A model
//! that fails to load is reported in [`SpawnedScene::failed`] and its instances are left out; the rest
//! of the scene still spawns. Skinned meshes of scene models are left out: they have no skeleton to pose
//! (use [`crate::spawn_animated`] for characters).
//!
//! # Lights
//!
//! Bevy's lights are physical too, so the conversion is a unit change and nothing else:
//!
//! | bridge | unit | Bevy | unit | formula |
//! |--------|------|------|------|---------|
//! | directional | lux | [`DirectionalLight::illuminance`] | lux | unchanged |
//! | point | candela | [`PointLight::intensity`] | lumens | `lumens = candela * 4π` ([`LUMENS_PER_CANDELA`]) |
//! | spot | candela | [`SpotLight::intensity`] | lumens | `lumens = candela * 4π`, whatever the cone |
//!
//! The factor for spots is the one Bevy's own glTF loader uses: Bevy's spot light intensity is the power
//! it would have if it shone in every direction, and the cone only cuts it off. Colour is linear RGB.
//! A light shines along its local -Z in both, the transform's scale is ignored, and a range of `None`
//! becomes [`SpawnOptions::unbounded_light_range`].
//!
//! # Exposure
//!
//! Lights are in photometric units and Bevy exposes them with the camera's [`Exposure`]: put an
//! `Exposure` on your camera that suits the scene (`Exposure::SUNLIGHT` for a day scene lit by tens of
//! thousands of lux, `Exposure::INDOOR` for lamps; the default is Blender's). This crate does not pick
//! one and has no exposure scale of its own. What is relative in the bridge (emissive materials, the
//! sky) is handled so that 1.0 reads as white: emissive colours are not exposed by default, so they
//! need nothing, and the sky takes [`SpawnOptions::environment_scale`], which you set to
//! [`white_luminance`] of your camera's exposure.
//!
//! # Sky
//!
//! [`rb::Environment`] is returned as a [`Sky`] in [`SpawnedScene::sky`]; it belongs on a camera, so
//! apply it with [`Sky::apply`] (or [`Bridge::spawn_scene`](crate::Bridge::spawn_scene) followed by
//! the same call). A flat colour becomes the camera's clear colour, shown as it is (the clear colour
//! is not exposed; 1.0 is display white), plus an ambient light of that colour at
//! `colour * environment scale` cd/m². An equirectangular picture is resampled into a cube map for a
//! [`Skybox`] and, with [`SpawnOptions::sky_lighting`], a [`GeneratedEnvironmentMapLight`]; both use
//! `intensity * environment scale` cd/m² as their brightness.

use std::collections::HashMap;

use bevy::asset::RenderAssetUsages;
use bevy::camera::ClearColorConfig;
use bevy::light::{AmbientLight, GeneratedEnvironmentMapLight, Skybox};
use bevy::prelude::*;
use bevy::render::render_resource::{
    Extent3d, TextureDimension, TextureFormat, TextureViewDescriptor, TextureViewDimension,
};
use rewrite_bridge as rb;

use crate::model::{
    instance_transform, is_mirrored, mirrored_material, prepare_parts, BridgeAssets, MeshPart,
    SpawnOptions,
};

/// Lumens per candela for a point or spot light: the light's power if it shone equally in every
/// direction (4π steradians)
pub const LUMENS_PER_CANDELA: f32 = 4.0 * std::f32::consts::PI;

/// Marks the root entity of a spawned scene
#[derive(Component, Debug, Clone)]
pub struct BridgeScene {
    pub name: String,
}

/// Marks the entity of one placement, with its index in [`rb::Scene::instances`] and the model it
/// places (an index into [`rb::Scene::models`])
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeInstance {
    pub index: usize,
    pub model: usize,
}

/// Marks the entity of a light, with its index in [`rb::Scene::lights`]
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeLight(pub usize);

/// Where [`spawn_scene`] gets the models of [`rb::SceneModel::Asset`] references
pub trait ModelSource {
    /// Loads the model an asset reference names
    fn load_model(&self, asset: &rb::AssetRef) -> Result<rb::Model, rb::BoxError>;
}

/// Closures work as sources: `|asset: &AssetRef| provider.load(&asset.asset)`
impl<F> ModelSource for F
where
    F: Fn(&rb::AssetRef) -> Result<rb::Model, rb::BoxError>,
{
    fn load_model(&self, asset: &rb::AssetRef) -> Result<rb::Model, rb::BoxError> {
        self(asset)
    }
}

/// A [`ModelSource`] over [`rb::Provider`]s: references without a provider go to the first (the one
/// that gave the scene), references naming a game go to the provider whose [`rb::Provider::game`] matches
#[derive(Default)]
pub struct Providers<'a> {
    providers: Vec<&'a dyn rb::Provider>,
}

impl<'a> Providers<'a> {
    /// `local` is the provider the scene came from
    pub fn new(local: &'a dyn rb::Provider) -> Self {
        Self {
            providers: vec![local],
        }
    }

    /// Adds another game's provider, for scenes that mix games
    pub fn with(mut self, other: &'a dyn rb::Provider) -> Self {
        self.providers.push(other);
        self
    }
}

impl ModelSource for Providers<'_> {
    fn load_model(&self, asset: &rb::AssetRef) -> Result<rb::Model, rb::BoxError> {
        let provider = match &asset.provider {
            None => self.providers.first(),
            Some(game) => self.providers.iter().find(|p| p.game() == game),
        };
        match provider {
            Some(p) => p.load(&asset.asset),
            None => Err(format!("no provider for {:?}", asset.provider).into()),
        }
    }
}

/// A [`ModelSource`] for scenes whose models are all inline: every reference fails
pub struct NoSource;

impl ModelSource for NoSource {
    fn load_model(&self, asset: &rb::AssetRef) -> Result<rb::Model, rb::BoxError> {
        Err(format!("no source for asset {:?}", asset.asset).into())
    }
}

/// The sky of a scene, ready to put on a camera with [`Sky::apply`]
#[derive(Debug, Clone)]
pub enum Sky {
    /// A flat colour, linear RGB: relative radiance times the environment scale
    Color {
        clear: Color,
        ambient: Color,
        ambient_brightness: f32,
    },
    /// A cube map of the picture
    Cubemap {
        image: Handle<Image>,
        brightness: f32,
        lighting: bool,
    },
}

impl Sky {
    /// Puts the sky on a camera entity (see the module docs for what each kind sets)
    pub fn apply(&self, commands: &mut Commands, camera: Entity) {
        match self {
            Sky::Color {
                clear,
                ambient,
                ambient_brightness,
            } => {
                let clear = *clear;
                commands
                    .entity(camera)
                    .entry::<Camera>()
                    .and_modify(move |mut c| c.clear_color = ClearColorConfig::Custom(clear));
                commands.entity(camera).insert(AmbientLight {
                    color: *ambient,
                    brightness: *ambient_brightness,
                    ..default()
                });
            }
            Sky::Cubemap {
                image,
                brightness,
                lighting,
            } => {
                commands.entity(camera).insert(Skybox {
                    image: Some(image.clone()),
                    brightness: *brightness,
                    rotation: Quat::IDENTITY,
                });
                if *lighting {
                    commands
                        .entity(camera)
                        .insert(GeneratedEnvironmentMapLight {
                            environment_map: image.clone(),
                            intensity: *brightness,
                            ..default()
                        });
                }
            }
        }
    }
}

/// What [`spawn_scene`] created
#[derive(Debug, Clone)]
pub struct SpawnedScene {
    pub root: Entity,
    /// The placements that spawned, in the scene's order (those of failed models are missing)
    pub instances: Vec<Entity>,
    /// The lights, in the scene's order
    pub lights: Vec<Entity>,
    /// Per scene model, its uploaded meshes; empty for a model that failed or that no instance uses
    pub models: Vec<Vec<MeshPart>>,
    /// Scene models that could not be loaded, with the reason
    pub failed: Vec<(usize, String)>,
    pub sky: Option<Sky>,
}

impl From<SpawnedScene> for Entity {
    fn from(s: SpawnedScene) -> Entity {
        s.root
    }
}

/// A bridge light as Bevy's: see the module docs for the units
pub fn spawn_light(commands: &mut Commands, light: &rb::Light, options: &SpawnOptions) -> Entity {
    // A light is placed and aimed, not scaled
    let t = instance_transform(&light.transform);
    let transform = Transform {
        translation: t.translation,
        rotation: t.rotation,
        scale: Vec3::ONE,
    };
    let [r, g, b] = light.color;
    let color = Color::linear_rgb(r, g, b);
    let mut e = commands.spawn((
        Name::new(light.name.clone()),
        transform,
        Visibility::default(),
    ));
    match light.kind {
        rb::LightKind::Directional => {
            e.insert(DirectionalLight {
                color,
                illuminance: light.intensity,
                shadow_maps_enabled: options.light_shadows,
                ..default()
            });
        }
        rb::LightKind::Point { range } => {
            e.insert(PointLight {
                color,
                intensity: light.intensity * LUMENS_PER_CANDELA,
                range: range.unwrap_or(options.unbounded_light_range),
                radius: 0.0,
                shadow_maps_enabled: options.light_shadows,
                ..default()
            });
        }
        rb::LightKind::Spot {
            range,
            inner_cone_angle,
            outer_cone_angle,
        } => {
            e.insert(SpotLight {
                color,
                intensity: light.intensity * LUMENS_PER_CANDELA,
                range: range.unwrap_or(options.unbounded_light_range),
                radius: 0.0,
                inner_angle: inner_cone_angle,
                outer_angle: outer_cone_angle,
                shadow_maps_enabled: options.light_shadows,
                ..default()
            });
        }
        // Kinds added to the core crate later
        #[allow(unreachable_patterns)]
        other => warn!("{}: light kind {other:?} is not supported", light.name),
    }
    e.id()
}

/// The lookup of a pixel of a linear-light image, for [`equirect_to_cubemap`]
fn srgb_table() -> [f32; 256] {
    let mut t = [0.0; 256];
    for (i, v) in t.iter_mut().enumerate() {
        let c = i as f32 / 255.0;
        *v = if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        };
    }
    t
}

fn encode_srgb(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let s = if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round() as u8
}

/// The direction a GPU samples a cube map in for texel (`i`, `j`) of a face of `size` texels, with the
/// faces in the usual order (+X, -X, +Y, -Y, +Z, -Z). Cube maps are left-handed, so Bevy's skybox and
/// environment lighting sample them with the world direction's z negated: the world direction of a texel
/// is this one with z flipped ([`equirect_to_cubemap`] does that).
pub fn cube_direction(face: usize, i: u32, j: u32, size: u32) -> Vec3 {
    let u = (i as f32 + 0.5) / size as f32 * 2.0 - 1.0;
    let v = (j as f32 + 0.5) / size as f32 * 2.0 - 1.0;
    match face {
        0 => Vec3::new(1.0, -v, -u),
        1 => Vec3::new(-1.0, -v, u),
        2 => Vec3::new(u, 1.0, v),
        3 => Vec3::new(u, -1.0, -v),
        4 => Vec3::new(u, -v, 1.0),
        _ => Vec3::new(-u, -v, -1.0),
    }
    .normalize()
}

/// Where a view direction lands in an equirectangular picture, as `(u, v)` in 0..=1 from the left
/// and top edges: the mapping [`rb::Environment::Equirectangular`] documents
pub fn equirect_uv(d: Vec3) -> (f32, f32) {
    let d = d.normalize();
    (
        0.5 + d.x.atan2(-d.z) / std::f32::consts::TAU,
        0.5 - d.y.clamp(-1.0, 1.0).asin() / std::f32::consts::PI,
    )
}

/// Resamples an equirectangular sRGB picture into a cube map image (six `size` x `size` faces,
/// bilinear in linear light, `Rgba8UnormSrgb`), the form Bevy's [`Skybox`] takes. World directions
/// follow the core crate's mapping, with Bevy's left-handed cube map convention taken care of.
pub fn equirect_to_cubemap(picture: &rb::Image, size: u32, usage: RenderAssetUsages) -> Image {
    let table = srgb_table();
    let (w, h) = (picture.width as usize, picture.height as usize);
    let texel =
        |x: usize, y: usize, k: usize| table[usize::from(picture.rgba[(y * w + x) * 4 + k])];
    let mut data = Vec::with_capacity(6 * (size as usize).pow(2) * 4);
    for face in 0..6 {
        for j in 0..size {
            for i in 0..size {
                // The cube map is sampled with the world direction's z negated, so a texel holds the picture
                // in the direction with z flipped
                let sampled = cube_direction(face, i, j, size);
                let (u, v) = equirect_uv(Vec3::new(sampled.x, sampled.y, -sampled.z));
                // Texel centres sit at half-integers; the picture wraps around horizontally
                let (fx, fy) = (
                    u * w as f32 - 0.5,
                    (v * h as f32 - 0.5).clamp(0.0, h as f32 - 1.0),
                );
                let (x0, y0) = (fx.floor(), fy.floor());
                let (tx, ty) = (fx - x0, fy - y0);
                let xa = (x0 as i64).rem_euclid(w as i64) as usize;
                let xb = (x0 as i64 + 1).rem_euclid(w as i64) as usize;
                let (ya, yb) = (y0 as usize, (y0 as usize + 1).min(h - 1));
                for k in 0..3 {
                    let top = texel(xa, ya, k) * (1.0 - tx) + texel(xb, ya, k) * tx;
                    let bottom = texel(xa, yb, k) * (1.0 - tx) + texel(xb, yb, k) * tx;
                    data.push(encode_srgb(top * (1.0 - ty) + bottom * ty));
                }
                data.push(255);
            }
        }
    }
    let mut image = Image::new(
        Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 6,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        usage,
    );
    image.texture_view_descriptor = Some(TextureViewDescriptor {
        dimension: Some(TextureViewDimension::Cube),
        ..default()
    });
    image
}

fn sky(
    environment: rb::Environment,
    assets: &mut BridgeAssets,
    options: &SpawnOptions,
) -> Option<Sky> {
    let scale = options.environment_scale;
    match environment {
        rb::Environment::Color([r, g, b]) => Some(Sky::Color {
            clear: Color::linear_rgb(r, g, b),
            ambient: Color::linear_rgb(r, g, b),
            ambient_brightness: scale,
        }),
        rb::Environment::Equirectangular { image, intensity } => {
            // A face covers a quarter of the picture's width; light generation wants powers of two
            let largest = options.sky_face_size.clamp(16, 4096);
            let size = 1u32 << (image.width / 4).clamp(16, largest).ilog2();
            let cube = equirect_to_cubemap(&image, size, RenderAssetUsages::RENDER_WORLD);
            Some(Sky::Cubemap {
                image: assets.images.add(cube),
                brightness: intensity * scale,
                lighting: options.sky_lighting,
            })
        }
        other => {
            warn!("environment {other:?} is not supported");
            None
        }
    }
}

/// Spawns a scene; see the module docs. `source` loads the models of asset references. Fails only if
/// the scene breaks the core crate's rules ([`rb::Scene::validate`]).
pub fn spawn_scene(
    commands: &mut Commands,
    assets: &mut BridgeAssets,
    mut scene: rb::Scene,
    source: &dyn ModelSource,
    options: &SpawnOptions,
) -> Result<SpawnedScene, rb::Invalid> {
    scene.validate()?;

    let root = commands
        .spawn((
            Name::new(scene.name.clone()),
            BridgeScene {
                name: scene.name.clone(),
            },
            Transform::default(),
            Visibility::default(),
        ))
        .id();

    let mut used = vec![false; scene.models.len()];
    for inst in &scene.instances {
        used[inst.model] = true;
    }

    // Upload each distinct model once. References to the same asset share the first one's handles.
    let mut models: Vec<Vec<MeshPart>> = vec![Vec::new(); scene.models.len()];
    let mut failed = Vec::new();
    let mut loaded: HashMap<(Option<String>, String), usize> = HashMap::new();
    for (i, scene_model) in std::mem::take(&mut scene.models).into_iter().enumerate() {
        if !used[i] {
            continue;
        }
        let model = match scene_model {
            rb::SceneModel::Inline(model) => model,
            rb::SceneModel::Asset(asset) => {
                let key = (asset.provider.clone(), asset.asset.clone());
                if let Some(&first) = loaded.get(&key) {
                    models[i] = models[first].clone();
                    continue;
                }
                match source
                    .load_model(&asset)
                    .and_then(|m| m.validate().map(|()| m).map_err(Into::into))
                {
                    Ok(model) => {
                        loaded.insert(key, i);
                        model
                    }
                    Err(e) => {
                        warn!(
                            "scene {}: model {i} ({}) failed: {e}",
                            scene.name, asset.asset
                        );
                        failed.push((i, e.to_string()));
                        continue;
                    }
                }
            }
            #[allow(unreachable_patterns)]
            other => {
                warn!(
                    "scene {}: model {i}: {other:?} is not supported",
                    scene.name
                );
                failed.push((i, "not supported".into()));
                continue;
            }
        };
        models[i] = prepare_parts(assets, model, options);
    }

    // Mirrored placements need their own material, once per material however many instances there are
    let mut mirrored: HashMap<AssetId<StandardMaterial>, Handle<StandardMaterial>> = HashMap::new();
    let mut handle_for = |assets: &mut BridgeAssets, part: &MeshPart, flip: bool| {
        if !flip {
            return part.material.clone();
        }
        mirrored
            .entry(part.material.id())
            .or_insert_with(|| mirrored_material(&mut assets.materials, &part.material))
            .clone()
    };

    let mut instances = Vec::with_capacity(scene.instances.len());
    for (index, inst) in scene.instances.iter().enumerate() {
        if failed.iter().any(|&(m, _)| m == inst.model) {
            continue;
        }
        let transform = instance_transform(&inst.transform);
        let flipped = is_mirrored(transform.scale);
        let marker = BridgeInstance {
            index,
            model: inst.model,
        };
        let parts = &models[inst.model];
        let e = match parts.as_slice() {
            // The common case: one mesh at the model's origin lives on the instance entity itself
            [part] if part.transform == Transform::IDENTITY => {
                let material = handle_for(assets, part, flipped);
                commands
                    .spawn((
                        marker,
                        transform,
                        Visibility::default(),
                        Mesh3d(part.mesh.clone()),
                        MeshMaterial3d(material),
                        ChildOf(root),
                    ))
                    .id()
            }
            _ => {
                let e = commands
                    .spawn((marker, transform, Visibility::default(), ChildOf(root)))
                    .id();
                for part in parts {
                    // The part's own transform may mirror too; the two cancel
                    let material =
                        handle_for(assets, part, flipped != is_mirrored(part.transform.scale));
                    commands.spawn((
                        part.transform,
                        Mesh3d(part.mesh.clone()),
                        MeshMaterial3d(material),
                        ChildOf(e),
                    ));
                }
                e
            }
        };
        instances.push(e);
    }

    let mut lights = Vec::with_capacity(scene.lights.len());
    for (i, light) in scene.lights.iter().enumerate() {
        let e = spawn_light(commands, light, options);
        commands.entity(e).insert((BridgeLight(i), ChildOf(root)));
        lights.push(e);
    }

    let sky = scene
        .environment
        .take()
        .and_then(|env| sky(env, assets, options));
    Ok(SpawnedScene {
        root,
        instances,
        lights,
        models,
        failed,
        sky,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::animation::graph::AnimationGraph;
    use bevy::animation::AnimationClip;
    use bevy::ecs::system::SystemState;
    use bevy::mesh::skinning::SkinnedMeshInverseBindposes;
    use bevy::render::render_resource::Face;
    use std::cell::RefCell;

    fn world() -> World {
        let mut world = World::new();
        world.init_resource::<Assets<Mesh>>();
        world.init_resource::<Assets<StandardMaterial>>();
        world.init_resource::<Assets<Image>>();
        world.init_resource::<Assets<SkinnedMeshInverseBindposes>>();
        world.init_resource::<Assets<AnimationClip>>();
        world.init_resource::<Assets<AnimationGraph>>();
        world
    }

    /// A one-mesh model: a triangle on the model's origin, with its own material
    fn prop(name: &str, node_offset: f32) -> rb::Model {
        let mut node = rb::Node::new("root", None);
        node.translation = [node_offset, 0.0, 0.0];
        rb::Model {
            name: name.into(),
            nodes: vec![node],
            meshes: vec![rb::Mesh {
                name: name.into(),
                positions: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                indices: vec![0, 1, 2],
                ..default()
            }],
            materials: vec![rb::Material {
                name: name.into(),
                ..default()
            }],
            skins: vec![],
        }
    }

    fn at(x: f32) -> rb::Transform {
        rb::Transform::from_translation([x, 0.0, 0.0])
    }

    fn try_spawn(
        world: &mut World,
        scene: rb::Scene,
        source: &dyn ModelSource,
    ) -> Result<SpawnedScene, rb::Invalid> {
        let mut state = SystemState::<(Commands, BridgeAssets)>::new(world);
        let (mut commands, mut assets) = state.get_mut(world).unwrap();
        let result = spawn_scene(
            &mut commands,
            &mut assets,
            scene,
            source,
            &SpawnOptions::default(),
        );
        state.apply(world);
        result
    }

    fn spawn(world: &mut World, scene: rb::Scene, source: &dyn ModelSource) -> SpawnedScene {
        try_spawn(world, scene, source).unwrap()
    }

    #[test]
    fn instances_of_one_model_share_its_mesh_and_material() {
        let mirrored = rb::Transform {
            scale: [-1.0, 1.0, 1.0],
            ..at(9.0)
        };
        let also_mirrored = rb::Transform {
            scale: [1.0, -1.0, -1.0],
            ..at(12.0)
        };
        let scene = rb::Scene {
            name: "forest".into(),
            models: vec![
                rb::SceneModel::Inline(prop("tree", 0.0)),
                rb::SceneModel::Inline(prop("rock", 0.0)),
            ],
            instances: vec![
                rb::Instance::new(0, at(1.0)),
                rb::Instance::new(0, at(2.0)),
                rb::Instance::new(1, at(3.0)),
                rb::Instance::new(0, mirrored),
                // Two negative axes turn, they do not mirror: the plain material
                rb::Instance::new(0, also_mirrored),
            ],
            ..default()
        };
        let mut world = world();
        let spawned = spawn(&mut world, scene, &NoSource);

        assert_eq!(spawned.instances.len(), 5);
        // Two models: two meshes and two materials whatever the instance count, plus one copy of the tree's
        // material for the mirrored instance
        assert_eq!(world.resource::<Assets<Mesh>>().len(), 2);
        assert_eq!(world.resource::<Assets<StandardMaterial>>().len(), 3);
        let handles = |e: Entity| {
            (
                world.get::<Mesh3d>(e).unwrap().0.clone(),
                world
                    .get::<MeshMaterial3d<StandardMaterial>>(e)
                    .unwrap()
                    .0
                    .clone(),
            )
        };
        let trees: Vec<_> = [0, 1, 3]
            .iter()
            .map(|&i| handles(spawned.instances[i]))
            .collect();
        assert_eq!(trees[0], trees[1]);
        assert_ne!(trees[0].0, handles(spawned.instances[2]).0);
        assert_eq!(spawned.models[0][0].mesh, trees[0].0);
        // The mirrored tree shares the mesh but culls the other way round, with one material for all mirrored trees
        assert_eq!(trees[2].0, trees[0].0);
        assert_ne!(trees[2].1, trees[0].1);
        let materials = world.resource::<Assets<StandardMaterial>>();
        assert_eq!(
            materials.get(&trees[0].1).unwrap().cull_mode,
            Some(Face::Back)
        );
        assert_eq!(
            materials.get(&trees[2].1).unwrap().cull_mode,
            Some(Face::Front)
        );

        // Placement and the marker, under the scene root; the mirror keeps its sign
        let e = spawned.instances[3];
        assert_eq!(
            world.get::<BridgeInstance>(e),
            Some(&BridgeInstance { index: 3, model: 0 })
        );
        assert_eq!(world.get::<ChildOf>(e).unwrap().parent(), spawned.root);
        let t = world.get::<Transform>(e).unwrap();
        assert_eq!(
            (t.translation, t.scale),
            (Vec3::new(9.0, 0.0, 0.0), Vec3::new(-1.0, 1.0, 1.0))
        );
        assert_eq!(
            world.get::<BridgeScene>(spawned.root).unwrap().name,
            "forest"
        );
        assert_eq!(handles(spawned.instances[4]), trees[0]);
    }

    #[test]
    fn models_off_the_origin_or_with_several_meshes_get_children() {
        let mut two = prop("pair", 2.0);
        two.meshes.push(two.meshes[0].clone());
        let scene = rb::Scene {
            name: "s".into(),
            models: vec![
                rb::SceneModel::Inline(two),
                rb::SceneModel::Inline(prop("unused", 0.0)),
            ],
            instances: vec![
                rb::Instance::new(0, at(10.0)),
                rb::Instance::new(0, at(20.0)),
            ],
            ..default()
        };
        let mut world = world();
        let spawned = spawn(&mut world, scene, &NoSource);
        // The unused model is never uploaded: two meshes of "pair", one material, shared by both instances
        assert_eq!(world.resource::<Assets<Mesh>>().len(), 2);
        assert!(spawned.models[1].is_empty());
        let e = spawned.instances[1];
        assert!(world.get::<Mesh3d>(e).is_none());
        let children = world.get::<Children>(e).unwrap();
        assert_eq!(children.len(), 2);
        for c in children.iter() {
            // The node's offset stays on the child, the instance's on the parent
            assert_eq!(
                world.get::<Transform>(c).unwrap().translation,
                Vec3::new(2.0, 0.0, 0.0)
            );
        }
        let first = world
            .get::<Mesh3d>(world.get::<Children>(spawned.instances[0]).unwrap()[0])
            .unwrap()
            .0
            .clone();
        let second = world.get::<Mesh3d>(children[0]).unwrap().0.clone();
        assert_eq!(first, second);
    }

    struct Counting {
        game: &'static str,
        loads: RefCell<Vec<String>>,
    }

    impl rb::Provider for Counting {
        fn game(&self) -> &str {
            self.game
        }
        fn list(&self, _: &str) -> Result<Vec<rb::AssetInfo>, rb::BoxError> {
            Ok(vec![])
        }
        fn load(&self, id: &str) -> Result<rb::Model, rb::BoxError> {
            self.loads.borrow_mut().push(id.to_string());
            if id == "broken" {
                let mut m = prop(id, 0.0);
                m.meshes[0].material = 7;
                return Ok(m);
            }
            if id == "missing" {
                return Err("not in this game".into());
            }
            Ok(prop(id, 0.0))
        }
    }

    #[test]
    fn asset_references_load_once_through_the_providers() {
        use std::rc::Rc;
        // Providers are borrowed for the call, so check through a source that shares them
        struct Shared(Rc<Counting>, Rc<Counting>);
        impl ModelSource for Shared {
            fn load_model(&self, a: &rb::AssetRef) -> Result<rb::Model, rb::BoxError> {
                Providers::new(&*self.0).with(&*self.1).load_model(a)
            }
        }
        let home = Rc::new(Counting {
            game: "Home",
            loads: RefCell::default(),
        });
        let away = Rc::new(Counting {
            game: "Away",
            loads: RefCell::default(),
        });
        let scene = rb::Scene {
            name: "mash-up".into(),
            models: vec![
                rb::SceneModel::Asset(rb::AssetRef::local("crate")),
                // The same asset named twice, once with its game spelled out: loaded once per reference spelling
                rb::SceneModel::Asset(rb::AssetRef::local("crate")),
                rb::SceneModel::Asset(rb::AssetRef::of("Away", "rock")),
                rb::SceneModel::Asset(rb::AssetRef::of("Home", "broken")),
                rb::SceneModel::Asset(rb::AssetRef::local("missing")),
                rb::SceneModel::Asset(rb::AssetRef::of("Nowhere", "x")),
            ],
            instances: (0..6).map(|m| rb::Instance::new(m, at(m as f32))).collect(),
            ..default()
        };
        let mut world = world();
        let spawned = spawn(&mut world, scene, &Shared(home.clone(), away.clone()));

        assert_eq!(*home.loads.borrow(), ["crate", "broken", "missing"]);
        assert_eq!(*away.loads.borrow(), ["rock"]);
        // The two references to the crate share handles, and the failures are listed, not fatal
        assert_eq!(spawned.models[0][0].mesh, spawned.models[1][0].mesh);
        assert_eq!(spawned.instances.len(), 3);
        assert_eq!(
            spawned.failed.iter().map(|f| f.0).collect::<Vec<_>>(),
            [3, 4, 5]
        );
        assert!(
            spawned.failed[0].1.contains("material 7"),
            "{:?}",
            spawned.failed[0]
        );
        assert_eq!(world.resource::<Assets<Mesh>>().len(), 2);
    }

    #[test]
    fn closures_are_sources_and_invalid_scenes_are_refused() {
        let scene = rb::Scene {
            name: "s".into(),
            models: vec![rb::SceneModel::Asset(rb::AssetRef::local("a"))],
            instances: vec![rb::Instance::new(0, at(0.0))],
            ..default()
        };
        let mut world = world();
        let spawned = spawn(
            &mut world,
            scene,
            &|a: &rb::AssetRef| -> Result<rb::Model, rb::BoxError> { Ok(prop(&a.asset, 0.0)) },
        );
        assert_eq!(spawned.instances.len(), 1);

        let bad = rb::Scene {
            instances: vec![rb::Instance::new(3, at(0.0))],
            ..default()
        };
        let result = try_spawn(&mut world, bad, &NoSource).map(|_| ());
        assert!(result.unwrap_err().0.contains("out of range"));
    }

    #[test]
    fn lights_keep_their_photometric_units() {
        let light = |kind, intensity| rb::Light {
            name: "l".into(),
            kind,
            color: [1.0, 0.5, 0.25],
            intensity,
            transform: at(1.0),
        };
        let scene = rb::Scene {
            name: "lit".into(),
            lights: vec![
                light(rb::LightKind::Directional, 100_000.0),
                light(rb::LightKind::Point { range: Some(12.0) }, 135.0),
                light(
                    rb::LightKind::Spot {
                        range: None,
                        inner_cone_angle: 0.2,
                        outer_cone_angle: 0.5,
                    },
                    1000.0,
                ),
            ],
            ..default()
        };
        let mut world = world();
        let spawned = spawn(&mut world, scene, &NoSource);
        let [sun, lamp, torch] = spawned.lights[..] else {
            panic!("three lights")
        };

        // Lux stay lux; a 135 cd bulb is 1696 lm, and a spot's cone does not change the factor
        let sun = world.get::<DirectionalLight>(sun).unwrap();
        assert_eq!(sun.illuminance, 100_000.0);
        assert_eq!(sun.color, Color::linear_rgb(1.0, 0.5, 0.25));
        let lamp = world.get::<PointLight>(lamp).unwrap();
        assert!((lamp.intensity - 135.0 * 4.0 * std::f32::consts::PI).abs() < 1e-3);
        assert!(
            (lamp.intensity - 1696.46).abs() < 0.01,
            "{}",
            lamp.intensity
        );
        assert_eq!(lamp.range, 12.0);
        let torch_light = world.get::<SpotLight>(torch).unwrap();
        assert!((torch_light.intensity - 1000.0 * LUMENS_PER_CANDELA).abs() < 1e-2);
        assert_eq!(
            (torch_light.inner_angle, torch_light.outer_angle),
            (0.2, 0.5)
        );
        assert_eq!(
            torch_light.range,
            SpawnOptions::default().unbounded_light_range
        );
        assert!(!torch_light.shadow_maps_enabled);
        assert_eq!(world.get::<BridgeLight>(torch), Some(&BridgeLight(2)));
        assert_eq!(world.get::<Transform>(torch).unwrap().translation, Vec3::X);
    }

    #[test]
    fn the_equirectangular_mapping_matches_the_core_crates() {
        let close =
            |a: (f32, f32), b: (f32, f32)| (a.0 - b.0).abs() < 1e-5 && (a.1 - b.1).abs() < 1e-5;
        // The viewer looks down -Z at the middle of the picture; turning toward +X moves right; up is the top row
        assert!(close(equirect_uv(Vec3::NEG_Z), (0.5, 0.5)));
        assert!(close(equirect_uv(Vec3::X), (0.75, 0.5)));
        assert!(close(equirect_uv(Vec3::Z), (1.0, 0.5)) || close(equirect_uv(Vec3::Z), (0.0, 0.5)));
        // At the poles only the row is defined
        assert!(equirect_uv(Vec3::Y).1.abs() < 1e-6);
        assert!((equirect_uv(Vec3::NEG_Y).1 - 1.0).abs() < 1e-6);
        // Faces look along +X, -X, +Y, -Y, +Z, -Z at their centres
        for (face, axis) in [
            Vec3::X,
            Vec3::NEG_X,
            Vec3::Y,
            Vec3::NEG_Y,
            Vec3::Z,
            Vec3::NEG_Z,
        ]
        .into_iter()
        .enumerate()
        {
            assert!(cube_direction(face, 1, 1, 4).dot(axis) > 0.7, "face {face}");
            assert!(cube_direction(face, 1, 1, 4)
                .abs_diff_eq(cube_direction(face, 1, 1, 4).normalize(), 1e-6));
        }
    }

    #[test]
    fn cubemaps_show_each_picture_direction_on_the_right_face() {
        // 8x4 picture: the column right of the middle (looking at -Z, toward +X) is red, the one left of it blue,
        // the top row white, the bottom row and everything else black
        let (w, h) = (8usize, 4usize);
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let px = match (x, y) {
                    (_, 0) => [255, 255, 255, 255],
                    (4, 1 | 2) => [255, 0, 0, 255],
                    (3, 1 | 2) => [0, 0, 255, 255],
                    _ => [0, 0, 0, 255],
                };
                rgba[(y * w + x) * 4..][..4].copy_from_slice(&px);
            }
        }
        let size = 16;
        let cube = equirect_to_cubemap(
            &rb::Image {
                width: 8,
                height: 4,
                rgba,
            },
            size,
            RenderAssetUsages::default(),
        );
        assert_eq!(cube.texture_descriptor.size.depth_or_array_layers, 6);
        assert_eq!(
            cube.texture_descriptor.format,
            TextureFormat::Rgba8UnormSrgb
        );
        assert_eq!(
            cube.texture_view_descriptor.as_ref().unwrap().dimension,
            Some(TextureViewDimension::Cube)
        );
        let data = cube.data.as_ref().unwrap();
        let texel = |face: usize, i: u32, j: u32| {
            let at = ((face * (size * size) as usize) + (j * size + i) as usize) * 4;
            [data[at], data[at + 1], data[at + 2]]
        };
        // Bevy samples cube maps with z negated, so the picture's middle (world -Z) is on the +Z face, where
        // the sampling direction's x grows with the column: +X, the red side, is at the face image's right
        let (red, blue) = (texel(4, size - 3, size / 2), texel(4, 2, size / 2));
        assert!(red[0] > 200 && red[2] < 60, "{red:?}");
        assert!(blue[2] > 200 && blue[0] < 60, "{blue:?}");
        // The +Y face is the white top row, the -Y face the black bottom
        assert_eq!(texel(2, size / 2, size / 2), [255, 255, 255]);
        assert_eq!(texel(3, size / 2, size / 2), [0, 0, 0]);
    }
}
