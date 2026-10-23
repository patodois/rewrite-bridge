//! Load any [rewrite-bridge](https://github.com/patodois/rewrite-bridge) provider's models, skins,
//! animations and scenes into a Bevy game, in a few lines.
//!
//! ```no_run
//! use bevy::prelude::*;
//! use rewrite_bridge::Provider;
//! use rewrite_bridge_bevy::*;
//!
//! # fn open_provider() -> Box<dyn Provider + Send + Sync> { unimplemented!() }
//! #[derive(Resource)]
//! struct Game(Box<dyn Provider + Send + Sync>);
//!
//! fn main() {
//!     App::new()
//!         .add_plugins((DefaultPlugins, RewriteBridgePlugin::default()))
//!         .insert_resource(Game(open_provider()))
//!         .add_systems(Startup, setup)
//!         .run();
//! }
//!
//! fn setup(mut bridge: Bridge, game: Res<Game>) {
//!     // A character with clips and sockets...
//!     let robot = bridge.spawn_animated(game.0.load_animated("robot").unwrap());
//!     // ...or a level, whose asset references load through the same provider
//!     let yard = bridge.spawn_scene(game.0.load_scene("yard").unwrap(), &Providers::new(game.0.as_ref())).unwrap();
//! }
//! ```
//!
//! # What it does
//!
//! - [`spawn_model`] / [`Bridge::spawn_model`]: a [`rb::Model`] as an entity per node, a [`Mesh3d`]
//!   per mesh (with normals, tangents and UVs), a [`StandardMaterial`] per material (alpha mode, sRGB
//!   and linear images, emissive) and a [`SkinnedMesh`](bevy::mesh::skinning::SkinnedMesh) per skinned
//!   mesh; see [`model`].
//! - [`spawn_animated`]: the same, plus the model's clips as Bevy [`AnimationClip`]s on an
//!   [`AnimationGraph`] you play by name with [`BridgeAnimations::play`], and its sockets as named
//!   child entities; see [`animated`].
//! - [`spawn_scene`]: a level, with the instances of each model sharing one mesh and one material,
//!   lights in physical units and the sky; see [`scene`].
//!
//! # Conventions
//!
//! Bridge models follow glTF 2.0 and so does Bevy: metres, +Y up, right-handed, counter-clockwise front
//! faces, a model's front toward +Z, quaternions as x, y, z, w, OpenGL normal maps, roughness in G and
//! metalness in B. Nothing is flipped or rotated on the way in, and no crate knows which game an asset
//! came from.
//!
//! Photometric values keep their units (lux and candela become lux and lumens; see [`scene`]). What is
//! relative in the bridge (emissive factors, the sky; 1.0 is reference white) maps to Bevy like this:
//!
//! ```text
//! emissive  = emissive_factor * emissive_strength * emissive_scale        (times the emissive image)
//! sky       = colour or picture * intensity * environment_scale           (cd/m², through the exposure)
//! ```
//!
//! Bevy leaves emissive colours out of the camera's exposure by default, so `emissive_scale` is 1.0 and a
//! factor of 1.0 is white under any exposure, as with Bevy's own glTF loader. The sky does go through the
//! exposure: set `environment_scale` to [`white_luminance`] of your camera's `Exposure` (the default is
//! that of Bevy's default exposure). There is no exposure scale of this crate's own: you choose the
//! camera's `Exposure` to suit your lights.
//!
//! # Memory
//!
//! Everything has a by-value form that moves vertices and pixels into Bevy instead of copying them
//! ([`spawn_model_owned`], [`spawn_animated_owned`], [`spawn_scene`], [`prepare_parts`]), and uploads
//! with [`RenderAssetUsages::RENDER_WORLD`](bevy::asset::RenderAssetUsages) so the CPU copies go
//! away once the GPU has them. A scene's inline models and loaded assets are moved one at a time.
//!
//! # Bevy features
//!
//! This crate depends on Bevy with default features off and asks only for `bevy_asset`, `bevy_log`,
//! `bevy_color`, `bevy_image`, `bevy_mesh`, `bevy_mikktspace`, `bevy_animation`, `bevy_camera`,
//! `bevy_light`, `bevy_render`, `bevy_pbr` and `std`. Your app adds the rest it wants (a window,
//! audio, input), so a game that wants no audio or gamepad support does not get it from here.

pub mod animated;
pub mod model;
pub mod scene;

use bevy::animation::graph::AnimationGraph;
use bevy::animation::{AnimationClip, AnimationPlayer, AnimationPlugin};
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use rewrite_bridge as rb;

pub use animated::*;
pub use model::*;
pub use scene::*;

/// The `rewrite-bridge` core crate, re-exported so the versions always match
pub use rewrite_bridge;

/// Adds Bevy's animation plugin if the app lacks it, and keeps the [`SpawnOptions`] that [`Bridge`]
/// uses. Add it next to `DefaultPlugins` (or at least Bevy's asset, mesh, image and PBR plugins).
#[derive(Default, Clone)]
pub struct RewriteBridgePlugin {
    pub options: SpawnOptions,
}

impl RewriteBridgePlugin {
    pub fn with_options(options: SpawnOptions) -> Self {
        Self { options }
    }
}

impl Plugin for RewriteBridgePlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<AnimationPlugin>() {
            app.add_plugins(AnimationPlugin);
        }
        app.insert_resource(self.options.clone());
    }
}

/// Everything a spawn needs, as one system parameter: write `fn setup(mut bridge: Bridge)` and call
/// its `spawn_*` methods with the models you loaded. The fields are public for the rest (`commands`
/// to adjust what was spawned, `options` to read the settings). It holds the asset collections mutably,
/// so a system that takes it should reach meshes, materials and images through `bridge.assets` rather
/// than asking for `ResMut<Assets<Mesh>>` as well, which Bevy rejects as a conflict.
#[derive(SystemParam)]
pub struct Bridge<'w, 's> {
    pub commands: Commands<'w, 's>,
    pub assets: BridgeAssets<'w>,
    pub animation: AnimationAssets<'w>,
    pub options: Res<'w, SpawnOptions>,
}

impl Bridge<'_, '_> {
    /// [`spawn_model_owned`] with the plugin's options; the result converts into the root [`Entity`]
    pub fn spawn_model(&mut self, model: rb::Model) -> SpawnedModel {
        spawn_model_owned(&mut self.commands, &mut self.assets, model, &self.options)
    }

    /// [`spawn_animated_owned`] with the plugin's options
    pub fn spawn_animated(&mut self, model: rb::AnimatedModel) -> SpawnedAnimated {
        spawn_animated_owned(
            &mut self.commands,
            &mut self.assets,
            &mut self.animation,
            model,
            &self.options,
        )
    }

    /// [`spawn_scene`] with the plugin's options. `source` loads asset references: a [`Providers`], a
    /// closure, or [`NoSource`].
    pub fn spawn_scene(
        &mut self,
        scene: rb::Scene,
        source: &dyn ModelSource,
    ) -> Result<SpawnedScene, rb::Invalid> {
        spawn_scene(
            &mut self.commands,
            &mut self.assets,
            scene,
            source,
            &self.options,
        )
    }
}

impl From<SpawnedModel> for Entity {
    fn from(s: SpawnedModel) -> Entity {
        s.root
    }
}

// The collections the plugin relies on, named so a missing plugin shows up in the type
#[allow(dead_code)]
type _Needs = (AnimationGraph, AnimationClip, AnimationPlayer);
