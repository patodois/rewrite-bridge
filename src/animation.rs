//! Animation clips: keyframed node transforms, with glTF 2.0's meaning.
//!
//! A clip is an [`Animation`] made of [`Channel`]s. Each channel animates one property (a
//! [`Target`]) of one node of a [`crate::Model`], addressed by index into `Model::nodes`. Clips
//! carry no model of their own; they travel next to it in an [`crate::AnimatedModel`].
//!
//! Conventions, all glTF's:
//!
//! - times are seconds from the start of the clip, `f32`;
//! - translations are metres, in the node's parent space (the node's own `translation`
//!   replaced, not added to); scales are unit-less; rotations are unit quaternions as x, y, z, w;
//! - a channel replaces the node's value for that property, and properties no channel touches
//!   keep the node's rest value;
//! - `values` is a flat list of `f32`, the way a glTF accessor stores it: a keyframe is 3 floats
//!   for translation and scale and 4 for rotation (see [`Target::components`]);
//! - [`Interpolation::Linear`] on a rotation is spherical (slerp), on the others it is a plain lerp;
//! - [`Interpolation::CubicSpline`] stores three elements per keyframe, in this order: in-tangent,
//!   value, out-tangent, exactly as glTF does (so `values` is three times as long);
//! - before the first keyframe a channel holds its first value and after the last it holds its last.

use std::collections::HashSet;

use crate::transform::quaternion_problem;
use crate::Invalid;

/// A clip: named, `duration_s` seconds long, animating any number of node properties at once.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Animation {
    pub name: String,
    /// Length of the clip in seconds. No channel has a keyframe after it. A looping player wraps
    /// at this time, so it may be longer than the last keyframe.
    pub duration_s: f32,
    pub channels: Vec<Channel>,
}

/// Which property of a node a channel animates.
///
/// Matches may need a wildcard arm: more targets can be added later without a breaking change.
/// glTF's morph-target `weights` are not here yet because [`crate::Mesh`] has no morph targets
/// for them to drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Target {
    /// `Node::translation`, 3 floats, metres.
    Translation,
    /// `Node::rotation`, 4 floats, unit quaternion as x, y, z, w.
    Rotation,
    /// `Node::scale`, 3 floats.
    Scale,
}

impl Target {
    /// Floats in one value of this target: 3 for translation and scale, 4 for rotation.
    pub const fn components(self) -> usize {
        match self {
            Target::Translation | Target::Scale => 3,
            Target::Rotation => 4,
        }
    }
}

/// How a channel fills the time between two keyframes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Interpolation {
    /// Holds the earlier keyframe's value until the next keyframe.
    Step,
    /// Linear (spherical for rotations). glTF's default.
    #[default]
    Linear,
    /// Hermite cubic spline with the keyframes' tangents. Needs at least two keyframes.
    CubicSpline,
}

/// The keyframes of one property of one node.
#[derive(Clone, Debug, PartialEq)]
pub struct Channel {
    /// Index into the model's `nodes`.
    pub node: usize,
    pub target: Target,
    pub interpolation: Interpolation,
    /// Keyframe times in seconds, strictly increasing, from 0 up to the clip's `duration_s`.
    pub times: Vec<f32>,
    /// Flat values, `times.len() * target.components()` floats, or three times that for
    /// [`Interpolation::CubicSpline`] (in-tangent, value, out-tangent per keyframe).
    pub values: Vec<f32>,
}

impl Channel {
    pub fn new(
        node: usize,
        target: Target,
        interpolation: Interpolation,
        times: Vec<f32>,
        values: Vec<f32>,
    ) -> Self {
        Self {
            node,
            target,
            interpolation,
            times,
            values,
        }
    }
}

impl Animation {
    /// Checks the clip against a model with `node_count` nodes:
    ///
    /// - `duration_s` is finite and not negative;
    /// - each channel's node is in range, and no two channels animate the same property of the
    ///   same node;
    /// - times are finite, not negative, strictly increasing and no later than `duration_s`, and
    ///   there is at least one (two for a cubic spline);
    /// - `values` holds exactly `times.len()` values (three times that for a cubic spline), all
    ///   finite;
    /// - rotation values are unit quaternions within [`crate::QUATERNION_TOLERANCE`]. A cubic
    ///   spline's tangents are exempt, as they are not rotations.
    pub fn validate(&self, node_count: usize) -> Result<(), Invalid> {
        let bad = |msg: String| Err(Invalid(format!("{}: {msg}", self.name)));
        if !self.duration_s.is_finite() || self.duration_s < 0.0 {
            return bad(format!(
                "duration {} s is negative or not finite",
                self.duration_s
            ));
        }
        let mut seen = HashSet::new();
        for (i, ch) in self.channels.iter().enumerate() {
            let chan = |msg: String| bad(format!("channel {i}: {msg}"));
            if ch.node >= node_count {
                return chan(format!("node {} out of range of {node_count}", ch.node));
            }
            if !seen.insert((ch.node, ch.target)) {
                return chan(format!(
                    "node {} already has a {:?} channel in this clip",
                    ch.node, ch.target
                ));
            }
            let cubic = ch.interpolation == Interpolation::CubicSpline;
            if ch.times.is_empty() || (cubic && ch.times.len() < 2) {
                return chan(format!(
                    "{} keyframes is too few for {:?}",
                    ch.times.len(),
                    ch.interpolation
                ));
            }
            if let Some(t) = ch.times.iter().find(|t| !t.is_finite() || **t < 0.0) {
                return chan(format!("time {t} is negative or not finite"));
            }
            if let Some(w) = ch.times.windows(2).find(|w| w[1] <= w[0]) {
                return chan(format!("times do not increase: {} then {}", w[0], w[1]));
            }
            let last = ch.times[ch.times.len() - 1];
            if last > self.duration_s {
                return chan(format!(
                    "last time {last} s is past the clip's {} s",
                    self.duration_s
                ));
            }
            let c = ch.target.components();
            let per_key = if cubic { 3 * c } else { c };
            if ch.values.len() != ch.times.len() * per_key {
                return chan(format!(
                    "{} floats for {} keyframes of {:?} {:?}, expected {}",
                    ch.values.len(),
                    ch.times.len(),
                    ch.interpolation,
                    ch.target,
                    ch.times.len() * per_key
                ));
            }
            if let Some(v) = ch.values.iter().find(|v| !v.is_finite()) {
                return chan(format!("value {v} is not finite"));
            }
            if ch.target == Target::Rotation {
                let skip_in_tangent = if cubic { c } else { 0 };
                for k in 0..ch.times.len() {
                    let at = k * per_key + skip_in_tangent;
                    let q: [f32; 4] = ch.values[at..at + 4].try_into().expect("4 floats");
                    if let Some(p) = quaternion_problem(&q) {
                        return chan(format!("keyframe {k}: {p}"));
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip() -> Animation {
        Animation {
            name: "walk".into(),
            duration_s: 2.0,
            channels: vec![
                Channel::new(
                    0,
                    Target::Translation,
                    Interpolation::Linear,
                    vec![0.0, 1.0, 2.0],
                    vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 2.0, 0.0, 0.0],
                ),
                Channel::new(
                    1,
                    Target::Rotation,
                    Interpolation::Step,
                    vec![0.0, 1.5],
                    vec![0.0, 0.0, 0.0, 1.0, 0.0, 1.0, 0.0, 0.0],
                ),
                Channel::new(
                    1,
                    Target::Scale,
                    Interpolation::CubicSpline,
                    vec![0.5, 1.0],
                    // in-tangent, value, out-tangent for each of two keyframes
                    vec![0.0; 18],
                ),
            ],
        }
    }

    fn fails(f: impl FnOnce(&mut Animation)) -> String {
        let mut a = clip();
        f(&mut a);
        a.validate(2).unwrap_err().0
    }

    #[test]
    fn valid_clips_pass() {
        assert_eq!(clip().validate(2), Ok(()));
        assert_eq!(Animation::default().validate(0), Ok(()));
        assert_eq!(Interpolation::default(), Interpolation::Linear);
        assert_eq!(Target::Rotation.components(), 4);
        assert_eq!(Target::Scale.components(), 3);
    }

    #[test]
    fn duration_must_be_a_length() {
        assert!(fails(|a| a.duration_s = -1.0).contains("negative or not finite"));
        assert!(fails(|a| a.duration_s = f32::NAN).contains("negative or not finite"));
    }

    #[test]
    fn node_must_exist() {
        assert!(fails(|a| a.channels[0].node = 2).contains("node 2 out of range of 2"));
    }

    #[test]
    fn a_property_is_animated_once() {
        let msg = fails(|a| {
            let dup = a.channels[0].clone();
            a.channels.push(dup);
        });
        assert!(
            msg.contains("channel 3") && msg.contains("already has a Translation"),
            "{msg}"
        );
    }

    #[test]
    fn needs_keyframes() {
        assert!(fails(|a| {
            a.channels[0].times.clear();
            a.channels[0].values.clear();
        })
        .contains("0 keyframes"));
        assert!(fails(|a| {
            a.channels[2].times.truncate(1);
            a.channels[2].values.truncate(9);
        })
        .contains("1 keyframes is too few for CubicSpline"));
    }

    #[test]
    fn times_must_be_sane_and_increase() {
        assert!(fails(|a| a.channels[0].times[0] = -0.5).contains("time -0.5 is negative"));
        assert!(fails(|a| a.channels[0].times[1] = f32::NAN).contains("not finite"));
        assert!(fails(|a| a.channels[0].times[1] = 0.0).contains("times do not increase: 0 then 0"));
        assert!(fails(|a| a.channels[0].times = vec![0.0, 1.5, 1.0])
            .contains("times do not increase: 1.5 then 1"));
    }

    #[test]
    fn times_stay_inside_the_duration() {
        assert!(fails(|a| a.duration_s = 1.9).contains("last time 2 s is past the clip's 1.9 s"));
        // A clip may be longer than its last keyframe.
        let mut a = clip();
        a.duration_s = 10.0;
        assert_eq!(a.validate(2), Ok(()));
    }

    #[test]
    fn value_counts_follow_times() {
        assert!(fails(|a| {
            a.channels[0].values.pop();
        })
        .contains("8 floats for 3 keyframes of Linear Translation, expected 9"));
        assert!(fails(|a| {
            a.channels[1].values.pop();
        })
        .contains("7 floats for 2 keyframes of Step Rotation, expected 8"));
        // Cubic splines need three elements per keyframe.
        assert!(fails(|a| {
            a.channels[2].values.truncate(6);
        })
        .contains("6 floats for 2 keyframes of CubicSpline Scale, expected 18"));
        assert!(fails(|a| a.channels[0].values[4] = f32::INFINITY).contains("not finite"));
    }

    #[test]
    fn rotations_are_normalised() {
        assert!(fails(|a| a.channels[1].values[7] = 2.0).contains("keyframe 1: quaternion"));
        assert!(fails(|a| a.channels[1].values[3] = 0.0).contains("length 0, not 1"));
    }

    #[test]
    fn cubic_tangents_are_not_checked_as_rotations() {
        let mut a = clip();
        // Keyframe layout: in-tangent (unnormalised), value, out-tangent (unnormalised).
        let (id, v) = ([0.0, 0.0, 0.0, 1.0], [0.0, 0.0, 0.0, 1.0]);
        let mut values = vec![5.0, 6.0, 7.0, 8.0];
        values.extend(id);
        values.extend([9.0, 9.0, 9.0, 9.0]);
        values.extend([5.0, 6.0, 7.0, 8.0]);
        values.extend(v);
        values.extend([9.0, 9.0, 9.0, 9.0]);
        a.channels[1] = Channel::new(
            1,
            Target::Rotation,
            Interpolation::CubicSpline,
            vec![0.0, 1.0],
            values.clone(),
        );
        assert_eq!(a.validate(2), Ok(()));
        // ...but the value in the middle still is.
        values[4] = 3.0;
        a.channels[1].values = values;
        assert!(a
            .validate(2)
            .unwrap_err()
            .0
            .contains("keyframe 0: quaternion"));
    }
}
