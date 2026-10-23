//! Animation checks, on top of what `Animation::validate` already guarantees: times strictly
//! increasing and finite, channels addressing real nodes, unit quaternions and the right number
//! of values per key (cubic splines included).

use std::collections::HashSet;

use super::math::{self, Mat4};
use super::report::{Location, Report, Severity};
use super::{
    ANIMATION_MAX_DURATION_S, ANIMATION_MIN_DURATION_S, POSE_EXTENT_RATIO, ROOT_MOTION_EXTREME_MPS,
    ROOT_MOTION_REPORT_MPS,
};
use crate::{AnimatedModel, Animation, Interpolation, Target};

/// Runs every clip check on an animated model that already passed `validate`.
pub(crate) fn check(am: &AnimatedModel, rep: &mut Report) {
    let nodes = &am.model.nodes;
    let rest = math::world_matrices(nodes);
    let markers = marker_nodes(am);
    let mut names = HashSet::new();
    for (ci, a) in am.animations.iter().enumerate() {
        let loc = Location::clip(&a.name);
        if a.name.is_empty() {
            rep.push(
                Severity::Warning,
                "animation.unnamed",
                loc.clone(),
                format!("clip {ci} has no name, so it cannot be looked up"),
            );
        } else if !names.insert(a.name.as_str()) {
            rep.push(
                Severity::Warning,
                "animation.duplicate_name",
                loc.clone(),
                format!(
                    "clip {ci} repeats the name {:?}; lookups find the first",
                    a.name
                ),
            );
        }
        if a.channels.is_empty() {
            rep.push(
                Severity::Warning,
                "animation.empty",
                loc.clone(),
                "the clip has no channels".into(),
            );
            continue;
        }
        check_duration(a, &loc, rep);
        check_channels(am, a, &loc, rep);
        check_root_motion(am, a, &rest, &loc, rep);
        check_pose_extent(am, a, &rest, &markers, &loc, rep);
    }
}

/// Duration. A clip of 0 s cannot loop or be sampled over time. One shorter than a millisecond
/// or longer than ten minutes is almost certainly milliseconds (or ticks) read as seconds.
fn check_duration(a: &Animation, loc: &Location, rep: &mut Report) {
    if a.duration_s <= 0.0 {
        rep.push(
            Severity::Warning,
            "animation.zero_duration",
            loc.clone(),
            "duration is 0 s, so a looping player has nothing to play".into(),
        );
    } else if a.duration_s < ANIMATION_MIN_DURATION_S || a.duration_s > ANIMATION_MAX_DURATION_S {
        rep.push(
            Severity::Warning,
            "animation.duration_suspect",
            loc.clone(),
            format!(
                "duration {} s is outside {ANIMATION_MIN_DURATION_S} to {ANIMATION_MAX_DURATION_S} s: \
                 milliseconds read as seconds?",
                a.duration_s
            ),
        );
    }
}

/// Channels. Neighbouring rotation keys in opposite hemispheres (q and -q are the same rotation)
/// make a player that blends component-wise, or does not take the shortest arc, spin the long way
/// round between them. A zero scale key collapses the node, makes its matrix singular and its
/// normals undefined. Both are aggregated per clip, since a bad exporter does it on many bones.
fn check_channels(am: &AnimatedModel, a: &Animation, loc: &Location, rep: &mut Report) {
    let (mut flip_channels, mut flips) = (0usize, 0usize);
    let (mut zero_channels, mut zeros) = (0usize, 0usize);
    let mut first_flip = None;
    let mut first_zero = None;
    for (chi, ch) in a.channels.iter().enumerate() {
        let c = ch.target.components();
        let value = |k: usize| -> &[f32] {
            let o = math::value_offset(ch, k);
            &ch.values[o..o + c]
        };
        match ch.target {
            Target::Rotation if ch.interpolation != Interpolation::Step => {
                let n = (1..ch.times.len())
                    .filter(|&k| {
                        let (p, q) = (value(k - 1), value(k));
                        p.iter().zip(q).map(|(a, b)| a * b).sum::<f32>() < 0.0
                    })
                    .inspect(|&k| {
                        first_flip.get_or_insert((chi, k));
                    })
                    .count();
                if n > 0 {
                    flip_channels += 1;
                    flips += n;
                }
            }
            Target::Scale => {
                let n = (0..ch.times.len())
                    .filter(|&k| value(k).contains(&0.0))
                    .inspect(|&k| {
                        first_zero.get_or_insert((chi, k));
                    })
                    .count();
                if n > 0 {
                    zero_channels += 1;
                    zeros += n;
                }
            }
            _ => {}
        }
    }
    let at = |first: Option<(usize, usize)>| -> (Location, String) {
        let Some((chi, k)) = first else {
            return (loc.clone(), String::new());
        };
        let node = a.channels[chi].node;
        let name = am.model.nodes.get(node).map_or("", |n| n.name.as_str());
        (
            loc.clone().with_channel(chi).with_node(node),
            format!(", first at channel {chi} (node {node} {name:?}) key {k}"),
        )
    };
    if flips > 0 {
        let (l, at) = at(first_flip);
        rep.push(
            Severity::Warning,
            "animation.quaternion_sign_flip",
            l,
            format!(
                "{flips} rotation key(s) in {flip_channels} channel(s) lie in the opposite \
                 hemisphere from the previous key{at}: players that do not take the shortest arc \
                 spin the long way round"
            ),
        );
    }
    if zeros > 0 {
        let (l, at) = at(first_zero);
        rep.push(
            Severity::Warning,
            "animation.zero_scale",
            l,
            format!("{zeros} scale key(s) in {zero_channels} channel(s) have a zero component{at}"),
        );
    }
}

/// Root motion. The farthest the root moves from its first key, per second of clip, in world
/// metres. A walk is 1 to 2 m/s, a run 4 to 10, a car up to 70; 100 m/s or more is a unit bug
/// (centimetres in the keys, metres in the model). The root is a node with no parent, or, when
/// there is only one such node (an armature), its children.
fn check_root_motion(
    am: &AnimatedModel,
    a: &Animation,
    rest: &[Mat4],
    loc: &Location,
    rep: &mut Report,
) {
    if a.duration_s <= 0.0 {
        return;
    }
    let nodes = &am.model.nodes;
    let parentless: Vec<usize> = (0..nodes.len())
        .filter(|&i| nodes[i].parent.is_none())
        .collect();
    let mut candidates = parentless.clone();
    if parentless.len() == 1 {
        candidates.extend((0..nodes.len()).filter(|&i| nodes[i].parent == Some(parentless[0])));
    }
    let mut best: Option<(f64, usize)> = None;
    for (chi, ch) in a.channels.iter().enumerate() {
        if ch.target != Target::Translation || !candidates.contains(&ch.node) {
            continue;
        }
        let parent = nodes[ch.node]
            .parent
            .and_then(|p| rest.get(p))
            .copied()
            .unwrap_or(math::IDENTITY);
        let at = |k: usize| -> [f64; 3] {
            let o = math::value_offset(ch, k);
            [0, 1, 2].map(|i| f64::from(ch.values[o + i]))
        };
        let p0 = at(0);
        let excursion = (1..ch.times.len())
            .map(|k| math::len3(math::transform_vector(&parent, math::sub3(at(k), p0))))
            .fold(0.0, f64::max);
        let speed = excursion / f64::from(a.duration_s);
        if best.is_none_or(|(s, _)| speed > s) {
            best = Some((speed, chi));
        }
    }
    let Some((speed, chi)) = best else { return };
    let node = a.channels[chi].node;
    let l = loc.clone().with_channel(chi).with_node(node);
    let name = &nodes[node].name;
    if speed >= ROOT_MOTION_EXTREME_MPS {
        rep.push(
            Severity::Warning,
            "animation.root_motion_extreme",
            l,
            format!(
                "node {node} ({name:?}) travels {speed:.1} m/s, over {ROOT_MOTION_EXTREME_MPS}: \
                 are the keys in centimetres?"
            ),
        );
    } else if speed >= ROOT_MOTION_REPORT_MPS {
        rep.push(
            Severity::Info,
            "animation.root_motion",
            l,
            format!("node {node} ({name:?}) travels {speed:.2} m/s"),
        );
    }
}

/// The nodes whose positions show how big the skeleton is: the joints of the skins, or, for a
/// model without skins, the nodes that carry meshes.
fn marker_nodes(am: &AnimatedModel) -> Vec<usize> {
    let mut v: Vec<usize> = if am.model.skins.is_empty() {
        am.model.meshes.iter().map(|m| m.node).collect()
    } else {
        am.model
            .skins
            .iter()
            .flat_map(|s| s.joints.iter().copied())
            .collect()
    };
    v.sort_unstable();
    v.dedup();
    v
}

fn spread(worlds: &[Mat4], markers: &[usize]) -> f64 {
    let (mut lo, mut hi) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
    for &i in markers {
        let m = &worlds[i];
        for k in 0..3 {
            lo[k] = lo[k].min(m[12 + k]);
            hi[k] = hi[k].max(m[12 + k]);
        }
    }
    math::len3(math::sub3(hi, lo))
}

/// Pose extent. Samples the clip at five times and compares the spread of the skeleton's joints
/// with the rest pose. A clip whose translations are in other units than the model (centimetres
/// against metres) stretches or shrinks the skeleton by the unit factor, which no per-value check
/// can see; a pose several times larger or smaller than the rest pose is that mistake.
fn check_pose_extent(
    am: &AnimatedModel,
    a: &Animation,
    rest: &[Mat4],
    markers: &[usize],
    loc: &Location,
    rep: &mut Report,
) {
    if markers.len() < 2 {
        return;
    }
    let base = spread(rest, markers);
    if !base.is_finite() || base <= 1e-3 {
        return;
    }
    let d = a.duration_s.max(0.0);
    let mut worst = (1.0f64, 0.0f32);
    for t in [0.0, 0.25 * d, 0.5 * d, 0.75 * d, d] {
        let ratio = spread(&math::posed_worlds(&am.model.nodes, a, t), markers) / base;
        let off = |r: f64| if r >= 1.0 { r } else { 1.0 / r.max(1e-12) };
        if off(ratio) > off(worst.0) {
            worst = (ratio, t);
        }
    }
    let (ratio, t) = worst;
    if ratio > POSE_EXTENT_RATIO || ratio < 1.0 / POSE_EXTENT_RATIO {
        rep.push(
            Severity::Warning,
            "animation.pose_extent_mismatch",
            loc.clone(),
            format!(
                "at {t:.2} s the skeleton spans {ratio:.2}x its rest pose ({base:.3} m): do the \
                 clip and the model use the same units?"
            ),
        );
    }
}
