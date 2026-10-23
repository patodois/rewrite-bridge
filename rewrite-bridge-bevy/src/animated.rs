//! Animation clips, attachment points and emissive strengths: [`rb::AnimatedModel`] in a Bevy world.
//!
//! [`spawn_animated_owned`] spawns the model like [`crate::spawn_model_owned`] and then:
//!
//! - turns every [`rb::Animation`] into a Bevy [`AnimationClip`], one curve per channel (see
//!   [`to_animation_clip`]), and builds an [`AnimationGraph`] with one clip node per animation;
//! - puts an [`AnimationPlayer`], the graph and a [`BridgeAnimations`] lookup on the root, and marks
//!   every node entity with an [`AnimationTargetId`] ([`target_id`]) and [`AnimatedBy`] the root, so
//!   the player drives the node [`Transform`]s;
//! - spawns every [`rb::Socket`] as a named child of its node ([`BridgeSocket`]), so whatever you
//!   parent to it follows the node through every clip;
//! - folds `emissive_strength` into the material's emissive factor, the way the core crate documents.
//!
//! # Playing a clip
//!
//! ```no_run
//! # use bevy::prelude::*;
//! # use rewrite_bridge_bevy::*;
//! fn play(mut players: Query<(&BridgeAnimations, &mut AnimationPlayer), Added<BridgeAnimations>>) {
//!     for (animations, mut player) in &mut players {
//!         animations.play(&mut player, "walk").expect("a clip named walk").repeat();
//!     }
//! }
//! ```
//!
//! # Conventions
//!
//! Times are seconds and values are glTF's (translation in the parent's space, rotation as x, y, z, w,
//! scale), which are also Bevy's [`Transform`] fields, so values go across unchanged. Linear rotation
//! is a slerp, cubic splines are glTF's (in-tangent, value, out-tangent per keyframe), a channel
//! holds its first value before its first keyframe and its last after its last, and a clip's length
//! is its `duration_s` even when the last keyframe is earlier, so looping wraps there.
//! Nodes a clip does not animate keep their rest transform; properties it does not animate keep the
//! value they have.

use std::collections::HashMap;
use std::ops::Deref;

use bevy::animation::animated_field;
use bevy::animation::animation_curves::AnimatableCurve;
use bevy::animation::gltf_curves::{CubicKeyframeCurve, CubicRotationCurve, SteppedKeyframeCurve};
use bevy::animation::graph::{AnimationGraph, AnimationGraphHandle, AnimationNodeIndex};
use bevy::animation::{
    ActiveAnimation, AnimatedBy, AnimationClip, AnimationPlayer, AnimationTargetId, VariableCurve,
};
use bevy::curve::{ConstantCurve, Interval, UnevenSampleAutoCurve};
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use rewrite_bridge as rb;

use crate::model::{spawn_model_owned, BridgeAssets, SpawnOptions, SpawnedModel};

/// The animation collections [`spawn_animated_owned`] adds to, as one system parameter
#[derive(SystemParam)]
pub struct AnimationAssets<'w> {
    pub clips: ResMut<'w, Assets<AnimationClip>>,
    pub graphs: ResMut<'w, Assets<AnimationGraph>>,
}

/// The [`AnimationTargetId`] of a model's node: what a clip's curve for that node is filed under.
/// Unique per node index (names are not: bones may share them), the same in every spawn of a model,
/// so the clips of one model can be played on any number of spawns.
pub fn target_id(node: usize) -> AnimationTargetId {
    let node = node.to_string();
    AnimationTargetId::from_iter(["rewrite-bridge-node", node.as_str()])
}

/// The clips of a spawned model, found by the name the provider gave them. Sits on the model's root,
/// next to its [`AnimationPlayer`].
#[derive(Component, Debug, Clone, Default)]
pub struct BridgeAnimations {
    clips: Vec<BridgeClip>,
}

/// One clip of [`BridgeAnimations`]
#[derive(Debug, Clone)]
pub struct BridgeClip {
    pub name: String,
    pub handle: Handle<AnimationClip>,
    /// The clip's node in the model's [`AnimationGraph`]
    pub node: AnimationNodeIndex,
    pub duration_s: f32,
}

impl BridgeAnimations {
    /// Every clip, in the order of [`rb::AnimatedModel::animations`]
    pub fn clips(&self) -> &[BridgeClip] {
        &self.clips
    }

    /// The clip with this name; the first if names repeat, as [`rb::AnimatedModel::animation`] does
    pub fn get(&self, name: &str) -> Option<&BridgeClip> {
        self.clips.iter().find(|c| c.name == name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.clips.iter().map(|c| c.name.as_str())
    }

    /// Starts the clip with this name on `player` (see [`AnimationPlayer::play`]) and returns it for
    /// `.repeat()`, `.set_speed(..)` and the like; `None` if there is no such clip
    pub fn play<'a>(
        &self,
        player: &'a mut AnimationPlayer,
        name: &str,
    ) -> Option<&'a mut ActiveAnimation> {
        self.get(name).map(|c| player.play(c.node))
    }
}

/// Marks an entity spawned from a [`rb::Socket`], a named attachment point; it is a child of its node
/// and its [`Transform`] is the socket's offset. Parent a weapon or a rider to it.
#[derive(Component, Debug, Clone)]
pub struct BridgeSocket {
    pub name: String,
}

/// The sockets of a spawned model by name; sits on the model's root
#[derive(Component, Debug, Clone, Default)]
pub struct BridgeSockets(pub HashMap<String, Entity>);

/// What [`spawn_animated_owned`] created; dereferences to the [`SpawnedModel`]
#[derive(Debug, Clone)]
pub struct SpawnedAnimated {
    pub model: SpawnedModel,
    /// One per animation, in the model's order
    pub clips: Vec<Handle<AnimationClip>>,
    pub sockets: HashMap<String, Entity>,
}

impl Deref for SpawnedAnimated {
    type Target = SpawnedModel;
    fn deref(&self) -> &SpawnedModel {
        &self.model
    }
}

impl From<SpawnedAnimated> for Entity {
    fn from(s: SpawnedAnimated) -> Entity {
        s.model.root
    }
}

fn vec3s(values: &[f32]) -> Vec<Vec3> {
    values
        .as_chunks::<3>()
        .0
        .iter()
        .map(|v| Vec3::from(*v))
        .collect()
}

fn quats(values: &[f32]) -> Vec<Vec4> {
    values
        .as_chunks::<4>()
        .0
        .iter()
        .map(|v| Vec4::from(*v))
        .collect()
}

/// A curve for everything but a cubic spline with two or more keyframes: constant, linear (slerp for
/// rotations) or stepped
macro_rules! simple_curve {
    ($ch:expr, $property:expr, $values:expr) => {{
        let ch: &rb::Channel = $ch;
        let values = $values;
        let times = ch.times.iter().copied();
        if ch.times.len() == 1 {
            // A lone cubic keyframe is in-tangent, value, out-tangent
            let value = values[if ch.interpolation == rb::Interpolation::CubicSpline {
                1
            } else {
                0
            }];
            Ok(VariableCurve::new(AnimatableCurve::new(
                $property,
                ConstantCurve::new(Interval::EVERYWHERE, value),
            )))
        } else if ch.interpolation == rb::Interpolation::Step {
            SteppedKeyframeCurve::new(times.zip(values))
                .map(|c| VariableCurve::new(AnimatableCurve::new($property, c)))
                .map_err(|e| format!("{e:?}"))
        } else {
            UnevenSampleAutoCurve::new(times.zip(values))
                .map(|c| VariableCurve::new(AnimatableCurve::new($property, c)))
                .map_err(|e| format!("{e:?}"))
        }
    }};
}

/// One channel as a Bevy curve on the node's [`Transform`], or why it cannot be one
pub fn to_variable_curve(ch: &rb::Channel) -> Result<VariableCurve, String> {
    let n = ch.times.len();
    let spline = ch.interpolation == rb::Interpolation::CubicSpline;
    let per_key = if spline { 3 } else { 1 };
    if n == 0 || ch.values.len() != n * per_key * ch.target.components() {
        return Err(format!(
            "{n} keyframes do not fit {} values",
            ch.values.len()
        ));
    }
    let times = ch.times.iter().copied();
    let cubic = spline && n >= 2;
    match ch.target {
        rb::Target::Translation => {
            let (property, values) = (animated_field!(Transform::translation), vec3s(&ch.values));
            if cubic {
                CubicKeyframeCurve::new(times, values)
                    .map(|c| VariableCurve::new(AnimatableCurve::new(property, c)))
                    .map_err(|e| format!("{e:?}"))
            } else {
                simple_curve!(ch, property, values)
            }
        }
        rb::Target::Scale => {
            let (property, values) = (animated_field!(Transform::scale), vec3s(&ch.values));
            if cubic {
                CubicKeyframeCurve::new(times, values)
                    .map(|c| VariableCurve::new(AnimatableCurve::new(property, c)))
                    .map_err(|e| format!("{e:?}"))
            } else {
                simple_curve!(ch, property, values)
            }
        }
        rb::Target::Rotation => {
            let (property, values) = (animated_field!(Transform::rotation), quats(&ch.values));
            if cubic {
                // Tangents are not rotations, so the spline works on the raw 4-vectors
                CubicRotationCurve::new(times, values)
                    .map(|c| VariableCurve::new(AnimatableCurve::new(property, c)))
                    .map_err(|e| format!("{e:?}"))
            } else {
                let values: Vec<Quat> = values
                    .into_iter()
                    .map(|q| Quat::from_vec4(q).normalize())
                    .collect();
                simple_curve!(ch, property, values)
            }
        }
        // Targets added to the core crate later: nothing to drive yet
        #[allow(unreachable_patterns)]
        other => Err(format!("{other:?} channels are not supported")),
    }
}

/// A bridge animation as a Bevy clip: one curve per channel, filed under [`target_id`] of the channel's
/// node, and the clip's duration set to `duration_s` (which may be longer than the last keyframe).
/// A channel Bevy cannot build a curve from is skipped with a warning; clips that pass
/// [`rb::Animation::validate`] have none.
pub fn to_animation_clip(animation: &rb::Animation) -> AnimationClip {
    let mut clip = AnimationClip::default();
    for ch in &animation.channels {
        match to_variable_curve(ch) {
            Ok(curve) => clip.add_variable_curve_to_target(target_id(ch.node), curve),
            Err(e) => warn!(
                "{}: channel of node {} skipped: {e}",
                animation.name, ch.node
            ),
        }
    }
    if animation.duration_s > clip.duration() {
        clip.set_duration(animation.duration_s);
    }
    clip
}

/// Folds `emissive_strength` into the emissive factors, as [`rb::AnimatedModel::emissive_factor`]
/// computes them
fn fold_emissive_strength(model: &mut rb::AnimatedModel) {
    let strengths = std::mem::take(&mut model.emissive_strength);
    for (m, s) in model.model.materials.iter_mut().zip(strengths) {
        m.emissive_factor = m.emissive_factor.map(|c| c * s);
    }
}

/// Spawns an animated model; see the module docs. The model's pixels and vertices are moved into
/// Bevy's assets. A model without clips gets no player or graph.
pub fn spawn_animated_owned(
    commands: &mut Commands,
    assets: &mut BridgeAssets,
    animation: &mut AnimationAssets,
    mut animated: rb::AnimatedModel,
    options: &SpawnOptions,
) -> SpawnedAnimated {
    fold_emissive_strength(&mut animated);
    let rb::AnimatedModel {
        model,
        animations,
        sockets,
        ..
    } = animated;
    let clips: Vec<Handle<AnimationClip>> = animations
        .iter()
        .map(|a| animation.clips.add(to_animation_clip(a)))
        .collect();
    let spawned = spawn_model_owned(commands, assets, model, options);
    let root = spawned.root;

    if !animations.is_empty() {
        let (graph, indices) = AnimationGraph::from_clips(clips.iter().cloned());
        for (i, &node) in spawned.nodes.iter().enumerate() {
            commands
                .entity(node)
                .insert((target_id(i), AnimatedBy(root)));
        }
        let table = BridgeAnimations {
            clips: animations
                .iter()
                .zip(&clips)
                .zip(indices)
                .map(|((a, handle), node)| BridgeClip {
                    name: a.name.clone(),
                    handle: handle.clone(),
                    node,
                    duration_s: a.duration_s,
                })
                .collect(),
        };
        commands.entity(root).insert((
            AnimationPlayer::default(),
            AnimationGraphHandle(animation.graphs.add(graph)),
            table,
        ));
    }

    let mut by_name = HashMap::with_capacity(sockets.len());
    for s in sockets {
        let Some(&node) = spawned.nodes.get(s.node) else {
            warn!("socket {}: node {} is out of range", s.name, s.node);
            continue;
        };
        let transform = crate::model::instance_transform(&s.offset);
        let e = commands
            .spawn((
                Name::new(s.name.clone()),
                BridgeSocket {
                    name: s.name.clone(),
                },
                transform,
                Visibility::default(),
                ChildOf(node),
            ))
            .id();
        by_name.insert(s.name, e);
    }
    commands.entity(root).insert(BridgeSockets(by_name.clone()));

    SpawnedAnimated {
        model: spawned,
        clips,
        sockets: by_name,
    }
}

/// [`spawn_animated_owned`], copying the model
pub fn spawn_animated(
    commands: &mut Commands,
    assets: &mut BridgeAssets,
    animation: &mut AnimationAssets,
    animated: &rb::AnimatedModel,
    options: &SpawnOptions,
) -> SpawnedAnimated {
    spawn_animated_owned(commands, assets, animation, animated.clone(), options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::animation::AnimationPlugin;
    use bevy::asset::AssetPlugin;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::mesh::skinning::SkinnedMeshInverseBindposes;
    use bevy::time::TimeUpdateStrategy;
    use std::time::Duration;

    fn close(a: Vec3, b: Vec3) -> bool {
        a.distance(b) < 1e-5
    }

    fn channel(
        target: rb::Target,
        interpolation: rb::Interpolation,
        times: &[f32],
        values: &[f32],
    ) -> rb::Channel {
        rb::Channel::new(1, target, interpolation, times.to_vec(), values.to_vec())
    }

    /// The curve's value at `t`, as the type the property has
    fn sample<T: Reflect>(curve: &VariableCurve, t: f32) -> T {
        *curve
            .0
            .sample_clamped(t)
            .downcast::<T>()
            .unwrap_or_else(|_| panic!("the curve's value type"))
    }

    #[test]
    fn linear_channels_keep_their_times_and_values() {
        let c = channel(
            rb::Target::Translation,
            rb::Interpolation::Linear,
            &[0.0, 1.0, 3.0],
            &[0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 2.0, 4.0, 0.0],
        );
        let curve = to_variable_curve(&c).unwrap();
        assert!(close(sample(&curve, 0.0), Vec3::ZERO));
        assert!(close(sample(&curve, 0.5), Vec3::new(1.0, 0.0, 0.0)));
        assert!(close(sample(&curve, 1.0), Vec3::new(2.0, 0.0, 0.0)));
        // Uneven spacing: halfway through the second span
        assert!(close(sample(&curve, 2.0), Vec3::new(2.0, 2.0, 0.0)));
        // Held before the first keyframe and after the last
        assert!(close(sample(&curve, -5.0), Vec3::ZERO));
        assert!(close(sample(&curve, 9.0), Vec3::new(2.0, 4.0, 0.0)));
    }

    #[test]
    fn linear_rotations_slerp_and_steps_hold() {
        let quarter = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        let q = |q: Quat| q.to_array();
        let mut values = q(Quat::IDENTITY).to_vec();
        values.extend(q(quarter));
        let c = channel(
            rb::Target::Rotation,
            rb::Interpolation::Linear,
            &[0.0, 2.0],
            &values,
        );
        let curve = to_variable_curve(&c).unwrap();
        let mid: Quat = sample(&curve, 1.0);
        assert!(
            mid.abs_diff_eq(Quat::from_rotation_y(std::f32::consts::FRAC_PI_4), 1e-5),
            "{mid}"
        );
        assert!(sample::<Quat>(&curve, 2.0).abs_diff_eq(quarter, 1e-5));

        let stepped = channel(
            rb::Target::Scale,
            rb::Interpolation::Step,
            &[0.0, 1.0],
            &[1.0, 1.0, 1.0, 3.0, 3.0, 3.0],
        );
        let curve = to_variable_curve(&stepped).unwrap();
        assert!(close(sample(&curve, 0.99), Vec3::ONE));
        assert!(close(sample(&curve, 1.0), Vec3::splat(3.0)));
    }

    #[test]
    fn cubic_splines_use_the_core_crates_tangent_layout() {
        // in-tangent, value, out-tangent per keyframe; the middle of a span of 2 s is
        // 0.5 p0 + 0.125 * dt * m_out0 + 0.5 p1 - 0.125 * dt * m_in1
        let values = [
            0.0, 0.0, 0.0, /**/ 0.0, 0.0, 0.0, /**/ 1.0, 0.0,
            0.0, // keyframe 0: value 0, out-tangent 1 per second
            0.0, 0.0, 0.0, /**/ 4.0, 0.0, 0.0, /**/ 0.0, 0.0,
            0.0, // keyframe 1: value 4, in-tangent 0
        ];
        let curve = to_variable_curve(&channel(
            rb::Target::Translation,
            rb::Interpolation::CubicSpline,
            &[0.0, 2.0],
            &values,
        ))
        .unwrap();
        assert!(close(sample(&curve, 0.0), Vec3::ZERO));
        assert!(close(sample(&curve, 2.0), Vec3::new(4.0, 0.0, 0.0)));
        let expected = 0.5 * 0.0 + 0.125 * 2.0 * 1.0 + 0.5 * 4.0;
        assert!(
            close(sample(&curve, 1.0), Vec3::new(expected, 0.0, 0.0)),
            "{}",
            sample::<Vec3>(&curve, 1.0)
        );

        // Rotation splines normalise their result
        let r = |x: f32, y: f32, z: f32, w: f32| [x, y, z, w];
        let mut rot = Vec::new();
        for k in [
            r(0.0, 0.0, 0.0, 0.0),
            r(0.0, 0.0, 0.0, 1.0),
            r(0.0, 0.0, 0.0, 0.0),
            r(0.0, 0.0, 0.0, 0.0),
            r(0.0, 1.0, 0.0, 0.0),
            r(0.0, 0.0, 0.0, 0.0),
        ] {
            rot.extend(k);
        }
        let curve = to_variable_curve(&channel(
            rb::Target::Rotation,
            rb::Interpolation::CubicSpline,
            &[0.0, 1.0],
            &rot,
        ))
        .unwrap();
        let mid: Quat = sample(&curve, 0.5);
        assert!((mid.length() - 1.0).abs() < 1e-5);
        // Half way between the identity and a half turn about y, by the spline with no tangents
        assert!(
            mid.abs_diff_eq(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2), 1e-5),
            "{mid}"
        );
    }

    #[test]
    fn single_keyframes_are_constant_and_bad_channels_are_refused() {
        let one = to_variable_curve(&channel(
            rb::Target::Scale,
            rb::Interpolation::Linear,
            &[0.5],
            &[2.0, 3.0, 4.0],
        ))
        .unwrap();
        assert!(close(sample(&one, -1.0), Vec3::new(2.0, 3.0, 4.0)));
        assert!(close(sample(&one, 100.0), Vec3::new(2.0, 3.0, 4.0)));
        // A lone cubic keyframe is in-tangent, value, out-tangent
        let lone = to_variable_curve(&channel(
            rb::Target::Translation,
            rb::Interpolation::CubicSpline,
            &[0.0],
            &[9.0, 9.0, 9.0, 1.0, 2.0, 3.0, 9.0, 9.0, 9.0],
        ))
        .unwrap();
        assert!(close(sample(&lone, 0.0), Vec3::new(1.0, 2.0, 3.0)));
        // Two keyframes but three values
        assert!(to_variable_curve(&channel(
            rb::Target::Translation,
            rb::Interpolation::Linear,
            &[0.0, 1.0],
            &[0.0; 3]
        ))
        .is_err());
        assert!(to_variable_curve(&channel(
            rb::Target::Translation,
            rb::Interpolation::Linear,
            &[],
            &[]
        ))
        .is_err());
    }

    fn robot() -> rb::AnimatedModel {
        let mut m = rb::AnimatedModel::new(rb::Model {
            name: "robot".into(),
            nodes: vec![
                rb::Node::new("root", None),
                rb::Node::new("hand", Some(0)),
                rb::Node::new("hand", Some(0)),
            ],
            materials: vec![rb::Material {
                emissive_factor: [0.5, 0.25, 0.0],
                ..default()
            }],
            ..default()
        });
        m.animations.push(rb::Animation {
            name: "reach".into(),
            // Longer than the last keyframe: a looping player wraps at 4 s
            duration_s: 4.0,
            channels: vec![
                rb::Channel::new(
                    1,
                    rb::Target::Translation,
                    rb::Interpolation::Linear,
                    vec![0.0, 2.0],
                    vec![0.0, 0.0, 0.0, 2.0, 0.0, 0.0],
                ),
                rb::Channel::new(
                    1,
                    rb::Target::Scale,
                    rb::Interpolation::Linear,
                    vec![0.0, 2.0],
                    vec![1.0, 1.0, 1.0, 1.0, 3.0, 1.0],
                ),
                rb::Channel::new(
                    2,
                    rb::Target::Translation,
                    rb::Interpolation::Step,
                    vec![0.0, 1.0],
                    vec![0.0, 0.0, 0.0, 0.0, 5.0, 0.0],
                ),
            ],
        });
        m.animations.push(rb::Animation {
            name: "idle".into(),
            duration_s: 1.0,
            channels: vec![],
        });
        m.sockets.push(rb::Socket::new(
            "grip",
            1,
            rb::Transform::from_translation([0.0, 0.1, 0.0]),
        ));
        m.emissive_strength = vec![4.0];
        m
    }

    #[test]
    fn clips_file_curves_under_their_nodes_and_keep_the_duration() {
        let m = robot();
        assert_eq!(m.validate(), Ok(()));
        let clip = to_animation_clip(&m.animations[0]);
        // Two properties of node 1, one of node 2, and nothing on node 0, although two nodes share a name
        assert_eq!(clip.curves_for_target(target_id(1)).map(Vec::len), Some(2));
        assert_eq!(clip.curves_for_target(target_id(2)).map(Vec::len), Some(1));
        assert!(clip.curves_for_target(target_id(0)).is_none());
        assert_ne!(target_id(1), target_id(2));
        assert_eq!(clip.duration(), 4.0);
        assert_eq!(to_animation_clip(&m.animations[1]).duration(), 1.0);
    }

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

    fn spawn_robot(world: &mut World) -> SpawnedAnimated {
        world
            .run_system_once(
                |mut commands: Commands, mut assets: BridgeAssets, mut anim: AnimationAssets| {
                    spawn_animated(
                        &mut commands,
                        &mut assets,
                        &mut anim,
                        &robot(),
                        &SpawnOptions::default(),
                    )
                },
            )
            .unwrap()
    }

    #[test]
    fn spawning_wires_clips_targets_sockets_and_emissive_strength() {
        let mut world = world();
        let spawned = spawn_robot(&mut world);
        let root = spawned.root;

        let table = world.get::<BridgeAnimations>(root).unwrap();
        assert_eq!(table.names().collect::<Vec<_>>(), ["reach", "idle"]);
        assert_eq!(table.get("reach").unwrap().duration_s, 4.0);
        assert!(table.get("nope").is_none());
        assert!(world.get::<AnimationPlayer>(root).is_some());
        let graph = world.get::<AnimationGraphHandle>(root).unwrap();
        assert!(world
            .resource::<Assets<AnimationGraph>>()
            .get(&graph.0)
            .is_some());
        assert_eq!(world.resource::<Assets<AnimationClip>>().len(), 2);

        for (i, &node) in spawned.nodes.iter().enumerate() {
            assert_eq!(world.get::<AnimationTargetId>(node), Some(&target_id(i)));
            assert_eq!(world.get::<AnimatedBy>(node).unwrap().0, root);
        }

        // The socket hangs under its node at its offset, and the root finds it by name
        let grip = spawned.sockets["grip"];
        assert_eq!(
            world.get::<ChildOf>(grip).unwrap().parent(),
            spawned.nodes[1]
        );
        assert_eq!(
            world.get::<Transform>(grip).unwrap().translation,
            Vec3::new(0.0, 0.1, 0.0)
        );
        assert_eq!(world.get::<Name>(grip).unwrap().as_str(), "grip");
        assert_eq!(world.get::<BridgeSockets>(root).unwrap().0["grip"], grip);

        // Emissive factor times strength
        let materials = world.resource::<Assets<StandardMaterial>>();
        assert_eq!(
            materials.get(&spawned.materials[0]).unwrap().emissive,
            LinearRgba::rgb(2.0, 1.0, 0.0)
        );
    }

    #[test]
    fn a_static_model_gets_no_player() {
        let mut world = world();
        let spawned = world
            .run_system_once(
                |mut commands: Commands, mut assets: BridgeAssets, mut anim: AnimationAssets| {
                    let still = rb::AnimatedModel::new(robot().model);
                    spawn_animated_owned(
                        &mut commands,
                        &mut assets,
                        &mut anim,
                        still,
                        &SpawnOptions::default(),
                    )
                },
            )
            .unwrap();
        assert!(world.get::<AnimationPlayer>(spawned.root).is_none());
        assert!(world.get::<AnimationTargetId>(spawned.nodes[0]).is_none());
        assert!(spawned.clips.is_empty());
    }

    /// The whole path: play a clip by name and let Bevy's animation systems move the nodes
    #[test]
    fn a_clip_played_by_name_moves_the_node_entities() {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            TransformPlugin,
            AnimationPlugin,
        ));
        app.init_asset::<Mesh>()
            .init_asset::<StandardMaterial>()
            .init_asset::<Image>()
            .init_asset::<SkinnedMeshInverseBindposes>();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            500,
        )));
        let spawned = spawn_robot(app.world_mut());
        {
            let world = app.world_mut();
            let table = world.get::<BridgeAnimations>(spawned.root).unwrap().clone();
            let mut player = world.get_mut::<AnimationPlayer>(spawned.root).unwrap();
            table
                .play(&mut player, "reach")
                .expect("a clip named reach")
                .repeat();
            assert!(table.play(&mut player, "nope").is_none());
        }
        // Run until the clip is 1.0 s in (the first updates only start Bevy's clock)
        let node = app
            .world()
            .get::<BridgeAnimations>(spawned.root)
            .unwrap()
            .get("reach")
            .unwrap()
            .node;
        let seek = |app: &App| {
            app.world()
                .get::<AnimationPlayer>(spawned.root)
                .unwrap()
                .animation(node)
                .unwrap()
                .seek_time()
        };
        for _ in 0..10 {
            if seek(&app) >= 1.0 {
                break;
            }
            app.update();
        }
        assert!((seek(&app) - 1.0).abs() < 1e-4, "{}", seek(&app));
        let hand = app.world().get::<Transform>(spawned.nodes[1]).unwrap();
        // 1.0 s into a 2 s ramp from 0 to 2 m, and the scale half-way from 1 to 3 on y
        assert!(
            close(hand.translation, Vec3::new(1.0, 0.0, 0.0)),
            "{}",
            hand.translation
        );
        assert!(
            close(hand.scale, Vec3::new(1.0, 2.0, 1.0)),
            "{}",
            hand.scale
        );
        // The stepped node jumped at 1.0 s; the unanimated root kept its rest transform
        let other = app.world().get::<Transform>(spawned.nodes[2]).unwrap();
        assert!(
            close(other.translation, Vec3::new(0.0, 5.0, 0.0)),
            "{}",
            other.translation
        );
        assert_eq!(
            app.world()
                .get::<Transform>(spawned.nodes[0])
                .unwrap()
                .translation,
            Vec3::ZERO
        );
    }
}
