//! Model-level checks: units, orientation, front; the rest are in `geometry` and `material`.

use super::geometry::{self, Aabb};
use super::math::{self, Mat4};
use super::report::{Facing, Location, Report, Severity};
use super::{material, Expect};
use crate::{Model, Socket};

use super::{
    CENTRE_OFFSET_REPORT_M, FEET_TOLERANCE_M, SCALE_MAX_EXTENT_M, SCALE_MIN_EXTENT_M,
    UPRIGHT_MIN_RATIO,
};

/// Words that name a place on the front of something. Names are split into lower-case words at
/// punctuation, digits and camelCase humps, and a word must match exactly (so `eyebrow` and
/// `backpack` match nothing).
const FRONT_WORDS: &[&str] = &[
    "muzzle",
    "eye",
    "eyes",
    "nose",
    "snout",
    "face",
    "headlight",
    "headlights",
];
/// Words that mean "front" or "rear" in a plain way. They only count when the whole name is made
/// of them and of `SIDE_WORDS`, because `superstructure_front` or `Door-Knocker-Front` name a
/// part's own front, not the model's.
const PLAIN_FRONT: &str = "front";
const PLAIN_REAR: &str = "rear";
const SIDE_WORDS: &[&str] = &[
    "front", "rear", "left", "right", "l", "r", "point", "marker", "locator", "dummy", "socket",
    "anchor",
];
/// Words that name a place on the back.
const REAR_WORDS: &[&str] = &["tail", "taillight", "taillights"];
/// A named point must be this far (metres, or this fraction of the horizontal extent, whichever
/// is greater) from the middle of the model for the facing to be read from it.
const FRONT_MIN_OFFSET_M: f64 = 0.03;
const FRONT_MIN_OFFSET_FRACTION: f64 = 0.02;
/// One horizontal axis must dominate the other by this factor, otherwise the facing is unknown.
const FRONT_AXIS_DOMINANCE: f64 = 1.5;

/// Runs every model check on a model that already passed `validate`.
pub(crate) fn check(model: &Model, sockets: &[Socket], expect: &Expect, rep: &mut Report) {
    let worlds = math::world_matrices(&model.nodes);
    let mut bounds = Aabb::new();
    let skin_mats = geometry::skin_matrices(model, &worlds);
    let usage = geometry::check_meshes(model, &worlds, &skin_mats, &mut bounds, rep);
    geometry::check_skins(model, &skin_mats, &usage, rep);
    material::check(model, &usage.material_used, rep);

    rep.measured.meshes = model.meshes.len();
    rep.measured.bounds = bounds.as_f32();
    let Some((lo, hi)) = bounds.get() else {
        if !model.meshes.is_empty() || model.nodes.is_empty() {
            rep.push(
                Severity::Warning,
                "geometry.no_vertices",
                Location::none(),
                "the model has no vertices that any triangle uses, so it has no size".into(),
            );
        }
        return;
    };
    let size = [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]];
    let is_character = expect
        .is_character
        .unwrap_or_else(|| model.meshes.iter().any(|m| m.skin.is_some()));

    check_scale(size, expect, rep);
    if is_character {
        check_orientation(lo, hi, size, rep);
    }
    check_front(model, sockets, &worlds, lo, hi, rep);
}

/// Units. glTF is in metres, so a model whose largest extent is under 5 cm or over 2 km is most
/// likely in centimetres, millimetres or ×100. A consumer places it by metres and gets a speck or
/// a skyscraper. The caller's `Expect` ranges catch the subtler cases.
fn check_scale(size: [f64; 3], expect: &Expect, rep: &mut Report) {
    let largest = size[0].max(size[1]).max(size[2]);
    if largest < f64::from(SCALE_MIN_EXTENT_M) {
        rep.push(
            Severity::Warning,
            "scale.too_small",
            Location::none(),
            format!(
                "largest extent is {largest:.4} m, under {SCALE_MIN_EXTENT_M} m: metres read as \
                 kilometres, or a unit that is too small?"
            ),
        );
    } else if largest > f64::from(SCALE_MAX_EXTENT_M) {
        rep.push(
            Severity::Warning,
            "scale.too_large",
            Location::none(),
            format!(
                "largest extent is {largest:.1} m, over {SCALE_MAX_EXTENT_M} m: centimetres read \
                 as metres, or a x100 scale?"
            ),
        );
    }
    if let Some((min, max)) = expect.height_m {
        if size[1] < f64::from(min) || size[1] > f64::from(max) {
            rep.push(
                Severity::Warning,
                "scale.height_out_of_range",
                Location::none(),
                format!(
                    "height (Y extent) is {:.3} m, expected {min} to {max} m",
                    size[1]
                ),
            );
        }
    }
    if let Some((min, max)) = expect.length_m {
        let length = size[0].max(size[2]);
        if length < f64::from(min) || length > f64::from(max) {
            rep.push(
                Severity::Warning,
                "scale.length_out_of_range",
                Location::none(),
                format!(
                    "length (larger of X and Z extent) is {length:.3} m, expected {min} to {max} m"
                ),
            );
        }
    }
}

/// Orientation of a character. The contract is +Y up with the feet at y = 0. A model that lies
/// down was exported Z-up, and one standing in the air or sunk into the ground is placed wrongly
/// by every consumer, which puts characters at their feet. The offset of the box centre from the
/// origin is reported because a character whose origin is not between its feet turns around a
/// pivot off to the side.
fn check_orientation(lo: [f64; 3], hi: [f64; 3], size: [f64; 3], rep: &mut Report) {
    let horizontal = size[0].max(size[2]);
    let ratio = if horizontal > 0.0 {
        size[1] / horizontal
    } else {
        f64::INFINITY
    };
    if ratio < f64::from(UPRIGHT_MIN_RATIO) {
        rep.push(
            Severity::Warning,
            "orientation.not_upright",
            Location::none(),
            format!(
                "height {:.3} m is under {UPRIGHT_MIN_RATIO} of the horizontal extent {horizontal:.3} m: \
                 is the model lying down (Z-up data read as Y-up)?",
                size[1]
            ),
        );
    } else if ratio < 1.0 {
        rep.push(
            Severity::Info,
            "orientation.wide",
            Location::none(),
            format!(
                "wider ({horizontal:.3} m) than tall ({:.3} m): a T-pose or a four-legged animal, or an axis mix-up",
                size[1]
            ),
        );
    }
    if lo[1].abs() > f64::from(FEET_TOLERANCE_M) {
        rep.push(
            Severity::Warning,
            "orientation.feet_off_ground",
            Location::none(),
            format!(
                "lowest point is at y = {:.3} m, more than {FEET_TOLERANCE_M} m from the ground",
                lo[1]
            ),
        );
    }
    let cx = (lo[0] + hi[0]) / 2.0;
    let cz = (lo[2] + hi[2]) / 2.0;
    if cx.abs().max(cz.abs()) > f64::from(CENTRE_OFFSET_REPORT_M) {
        rep.push(
            Severity::Info,
            "orientation.off_centre",
            Location::none(),
            format!("the bounding box centre is {cx:.3} m in X and {cz:.3} m in Z from the origin"),
        );
    }
}

fn words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in name.chars() {
        if !ch.is_alphabetic() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.extend(ch.to_lowercase());
        prev_lower = ch.is_lowercase();
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Front. The bridge says the front faces +Z, but a bare mesh cannot tell front from back, so
/// the facing is only read where the asset names it: nodes or sockets called muzzle, eye, nose,
/// face, headlight, or plainly "front" (positive), or tail, taillight, or plainly "rear"
/// (negative), vote with their offset from the middle of the model. Roots are skipped, and so is
/// "exhaust": it is at the rear of a tank, the front of a radial-engined aircraft and the middle
/// of a ship. Without such names, or when the votes do not clearly favour one axis, the facing is
/// `Unknown` and no finding is made. A consumer that spawns a tank facing backwards cannot be
/// helped by a guess, but can by a name.
fn check_front(
    model: &Model,
    sockets: &[Socket],
    worlds: &[Mat4],
    lo: [f64; 3],
    hi: [f64; 3],
    rep: &mut Report,
) {
    let centre = [(lo[0] + hi[0]) / 2.0, (lo[2] + hi[2]) / 2.0];
    let extent = (hi[0] - lo[0]).max(hi[2] - lo[2]);
    let mut vote = [0.0f64; 2];
    let mut named: Vec<String> = Vec::new();
    let mut add = |name: &str, p: [f64; 3]| {
        if !p.iter().all(|c| c.is_finite()) {
            return;
        }
        let w = words(name);
        let plain = !w.is_empty() && w.iter().all(|w| SIDE_WORDS.contains(&w.as_str()));
        let has = |list: &[&str]| w.iter().any(|w| list.contains(&w.as_str()));
        // "Eye-Back" is the back eye: a front word next to a back word says nothing.
        if has(FRONT_WORDS) && has(&["back", "rear", "behind"]) {
            return;
        }
        let sign = if has(FRONT_WORDS) || (plain && has(&[PLAIN_FRONT])) {
            1.0
        } else if has(REAR_WORDS) || (plain && has(&[PLAIN_REAR])) {
            -1.0
        } else {
            return;
        };
        vote[0] += sign * (p[0] - centre[0]);
        vote[1] += sign * (p[2] - centre[1]);
        if named.len() < 3 {
            named.push(name.to_string());
        }
    };
    for (n, w) in model.nodes.iter().zip(worlds) {
        // A root is usually named after the whole asset ("...FrontB"), which says nothing.
        if n.parent.is_some() {
            add(&n.name, [w[12], w[13], w[14]]);
        }
    }
    for s in sockets {
        if let Some(w) = worlds.get(s.node) {
            let m = math::mul(
                w,
                &math::from_trs(s.offset.translation, s.offset.rotation, s.offset.scale),
            );
            add(&s.name, [m[12], m[13], m[14]]);
        }
    }
    let min_offset = FRONT_MIN_OFFSET_M.max(FRONT_MIN_OFFSET_FRACTION * extent);
    let (ax, az) = (vote[0].abs(), vote[1].abs());
    let facing = if ax.max(az) < min_offset {
        Facing::Unknown
    } else if az >= FRONT_AXIS_DOMINANCE * ax {
        if vote[1] > 0.0 {
            Facing::PlusZ
        } else {
            Facing::MinusZ
        }
    } else if ax >= FRONT_AXIS_DOMINANCE * az {
        if vote[0] > 0.0 {
            Facing::PlusX
        } else {
            Facing::MinusX
        }
    } else {
        Facing::Unknown
    };
    rep.measured.facing = facing;
    if !matches!(facing, Facing::Unknown | Facing::PlusZ) {
        rep.push(
            Severity::Warning,
            "front.not_plus_z",
            Location::none(),
            format!(
                "names such as {named:?} put the front towards {facing}, but the bridge's front is +Z"
            ),
        );
    }
}
