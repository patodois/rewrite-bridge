//! Mesh and skin checks, and the world-space bounds of the default pose.

use super::math::{self, Mat4};
use super::report::{Location, Report, Severity};
use super::{
    BIND_POSE_TOLERANCE, DEGENERATE_AREA_RATIO, DEGENERATE_WARN_FRACTION, INVERSE_BIND_MIN_DET,
    NORMAL_TOLERANCE, WINDING_AMBIGUOUS_COS, WINDING_FLIPPED_FRACTION, WINDING_MIXED_FRACTION,
};
use crate::Model;

/// Running world-space bounds.
pub(crate) struct Aabb {
    lo: [f64; 3],
    hi: [f64; 3],
    any: bool,
}

impl Aabb {
    pub(crate) fn new() -> Self {
        Self {
            lo: [f64::INFINITY; 3],
            hi: [f64::NEG_INFINITY; 3],
            any: false,
        }
    }

    pub(crate) fn add(&mut self, p: [f64; 3]) {
        self.any = true;
        for ((lo, hi), c) in self.lo.iter_mut().zip(self.hi.iter_mut()).zip(p) {
            *lo = lo.min(c);
            *hi = hi.max(c);
        }
    }

    pub(crate) fn get(&self) -> Option<([f64; 3], [f64; 3])> {
        self.any.then_some((self.lo, self.hi))
    }

    pub(crate) fn as_f32(&self) -> Option<([f32; 3], [f32; 3])> {
        self.get()
            .map(|(lo, hi)| (lo.map(|c| c as f32), hi.map(|c| c as f32)))
    }
}

/// What the mesh pass learned that other checks use.
pub(crate) struct Usage {
    /// Per material: some mesh uses it.
    pub material_used: Vec<bool>,
    /// Per skin, per joint: some vertex gives it a positive weight.
    pub joint_used: Vec<Vec<bool>>,
}

/// `world(joint) * inverse_bind` for every joint of every skin: what moves a bind-space vertex
/// to the world in the node pose.
pub(crate) fn skin_matrices(model: &Model, worlds: &[Mat4]) -> Vec<Vec<Mat4>> {
    model
        .skins
        .iter()
        .map(|s| {
            s.joints
                .iter()
                .zip(&s.inverse_bind)
                .map(|(&j, ibm)| math::mul(&worlds[j], &math::from_f32(ibm)))
                .collect()
        })
        .collect()
}

fn count_non_finite<const N: usize>(v: &[[f32; N]]) -> usize {
    v.iter()
        .filter(|a| a.iter().any(|c| !c.is_finite()))
        .count()
}

fn p64(p: [f32; 3]) -> [f64; 3] {
    p.map(f64::from)
}

fn finite3(p: [f64; 3]) -> bool {
    p.iter().all(|c| c.is_finite())
}

/// Geometry. Each mesh is checked in its own space, so a node's transform (a mirror, say) cannot
/// hide a fault. Also folds the mesh's vertices, as the default pose puts them, into `bounds`
/// (only vertices a triangle uses: stray vertices are not drawn).
pub(crate) fn check_meshes(
    model: &Model,
    worlds: &[Mat4],
    skin_mats: &[Vec<Mat4>],
    bounds: &mut Aabb,
    rep: &mut Report,
) -> Usage {
    let mut usage = Usage {
        material_used: vec![false; model.materials.len()],
        joint_used: model
            .skins
            .iter()
            .map(|s| vec![false; s.joints.len()])
            .collect(),
    };
    for (mi, m) in model.meshes.iter().enumerate() {
        let loc = Location::mesh(mi);
        let n = m.positions.len();
        let tris = m.indices.len() / 3;
        rep.measured.vertices += n;
        rep.measured.triangles += tris;
        if let Some(u) = usage.material_used.get_mut(m.material) {
            *u = true;
        }

        // Non-finite numbers poison bounds, culling, skinning and tangents.
        let nf = [
            ("positions", count_non_finite(&m.positions)),
            ("normals", count_non_finite(&m.normals)),
            ("tangents", count_non_finite(&m.tangents)),
            ("uvs", count_non_finite(&m.uvs)),
        ];
        let bad: Vec<String> = nf
            .iter()
            .filter(|(_, c)| *c > 0)
            .map(|(a, c)| format!("{c} {a}"))
            .collect();
        if !bad.is_empty() {
            rep.push(
                Severity::Error,
                "geometry.non_finite",
                loc.clone(),
                format!(
                    "{} ({}) has NaN or infinite values: {}",
                    mi,
                    m.name,
                    bad.join(", ")
                ),
            );
        }
        if tris == 0 {
            if n > 0 {
                rep.push(
                    Severity::Warning,
                    "geometry.empty_mesh",
                    loc.clone(),
                    format!("{n} vertices but no triangles ({})", m.name),
                );
            }
            continue;
        }

        let mut used = vec![false; n];
        for &ix in &m.indices {
            if let Some(u) = used.get_mut(ix as usize) {
                *u = true;
            }
        }
        let unused = used.iter().filter(|u| !**u).count();
        if unused > 0 {
            rep.push(
                Severity::Info,
                "geometry.unused_vertices",
                loc.clone(),
                format!(
                    "{unused} of {n} vertices are used by no triangle ({})",
                    m.name
                ),
            );
        }

        fold_bounds(model, worlds, skin_mats, mi, &used, bounds, &mut usage);

        if m.normals.is_empty() {
            rep.push(
                Severity::Info,
                "geometry.no_normals",
                loc.clone(),
                format!("no normals ({})", m.name),
            );
        } else {
            let mut off = 0usize;
            let mut worst = 0.0f64;
            for nv in &m.normals {
                let nv = p64(*nv);
                if !finite3(nv) {
                    continue;
                }
                let d = (math::len3(nv) - 1.0).abs();
                if d > f64::from(NORMAL_TOLERANCE) {
                    off += 1;
                    worst = worst.max(d);
                }
            }
            if off > 0 {
                rep.push(
                    Severity::Warning,
                    "geometry.normals_not_unit",
                    loc.clone(),
                    format!(
                        "{off} of {} normals are not unit length (worst off by {worst:.4}, \
                         tolerance {NORMAL_TOLERANCE}) ({})",
                        m.normals.len(),
                        m.name
                    ),
                );
            }
        }

        check_triangles(m, &loc, rep);
        check_weights(model, mi, &loc, rep);
    }
    usage
}

fn fold_bounds(
    model: &Model,
    worlds: &[Mat4],
    skin_mats: &[Vec<Mat4>],
    mi: usize,
    used: &[bool],
    bounds: &mut Aabb,
    usage: &mut Usage,
) {
    let m = &model.meshes[mi];
    let skin = m
        .skin
        .and_then(|s| skin_mats.get(s).map(|mats| (s, mats)))
        .filter(|_| m.joints.len() == m.positions.len() && m.weights.len() == m.positions.len());
    let node = worlds.get(m.node).copied().unwrap_or(math::IDENTITY);
    for (v, p) in m.positions.iter().enumerate() {
        if !used[v] {
            continue;
        }
        let p = p64(*p);
        if !finite3(p) {
            continue;
        }
        let world = match skin {
            Some((s, mats)) => {
                let (j, w) = (&m.joints[v], &m.weights[v]);
                let mut acc = [0.0; 3];
                let mut total = 0.0;
                for k in 0..4 {
                    let wk = f64::from(w[k]);
                    if wk.is_nan() || wk <= 0.0 {
                        continue;
                    }
                    if let Some(mat) = mats.get(usize::from(j[k])) {
                        let q = math::transform_point(mat, p);
                        for c in 0..3 {
                            acc[c] += wk * q[c];
                        }
                        total += wk;
                        usage.joint_used[s][usize::from(j[k])] = true;
                    }
                }
                if total > 0.0 {
                    acc
                } else {
                    p
                }
            }
            None => math::transform_point(&node, p),
        };
        if finite3(world) {
            bounds.add(world);
        }
    }
}

/// Winding and degeneracy. Front faces wind counter-clockwise, so the cross product of two edges
/// points the same way as the surface normals. If most triangles disagree with their own vertex
/// normals, the winding was flipped (a handedness conversion applied to positions but not to the
/// index order), and with back-face culling the viewer sees the inside of the model. Degenerate
/// (zero-area) triangles are counted too, since they draw nothing and break tangent generation.
fn check_triangles(m: &crate::Mesh, loc: &Location, rep: &mut Report) {
    let (mut tris, mut degenerate, mut agree, mut disagree) = (0usize, 0usize, 0usize, 0usize);
    for t in m.indices.as_chunks::<3>().0 {
        let ix = [t[0] as usize, t[1] as usize, t[2] as usize];
        let (Some(a), Some(b), Some(c)) = (
            m.positions.get(ix[0]),
            m.positions.get(ix[1]),
            m.positions.get(ix[2]),
        ) else {
            continue;
        };
        let (a, b, c) = (p64(*a), p64(*b), p64(*c));
        if !(finite3(a) && finite3(b) && finite3(c)) {
            continue;
        }
        tris += 1;
        let (e1, e2, e3) = (math::sub3(b, a), math::sub3(c, a), math::sub3(c, b));
        let longest2 = math::dot3(e1, e1)
            .max(math::dot3(e2, e2))
            .max(math::dot3(e3, e3));
        let cross = math::cross3(e1, e2);
        let area2 = math::len3(cross);
        if longest2 == 0.0 || area2 <= DEGENERATE_AREA_RATIO * longest2 {
            degenerate += 1;
            continue;
        }
        if m.normals.is_empty() {
            continue;
        }
        let ns = ix.map(|i| p64(m.normals[i]));
        if !ns.iter().copied().all(finite3) {
            continue;
        }
        let sum = [
            ns[0][0] + ns[1][0] + ns[2][0],
            ns[0][1] + ns[1][1] + ns[2][1],
            ns[0][2] + ns[1][2] + ns[2][2],
        ];
        let sl = math::len3(sum);
        if sl < 1e-9 {
            continue;
        }
        let cos = math::dot3(cross, sum) / (area2 * sl);
        if cos > WINDING_AMBIGUOUS_COS {
            agree += 1;
        } else if cos < -WINDING_AMBIGUOUS_COS {
            disagree += 1;
        }
    }
    if tris > 0 && degenerate > 0 {
        let frac = degenerate as f64 / tris as f64;
        rep.push(
            if frac > DEGENERATE_WARN_FRACTION {
                Severity::Warning
            } else {
                Severity::Info
            },
            "geometry.degenerate_triangles",
            loc.clone(),
            format!(
                "{degenerate} of {tris} triangles ({:.1}%) have no area ({})",
                frac * 100.0,
                m.name
            ),
        );
    }
    let judged = agree + disagree;
    if judged > 0 {
        let frac = disagree as f64 / judged as f64;
        if frac > WINDING_FLIPPED_FRACTION {
            rep.push(
                Severity::Error,
                "geometry.winding_flipped",
                loc.clone(),
                format!(
                    "{disagree} of {judged} triangles ({:.0}%) wind against their normals ({}): \
                     the winding looks flipped",
                    frac * 100.0,
                    m.name
                ),
            );
        } else if frac > WINDING_MIXED_FRACTION {
            rep.push(
                Severity::Info,
                "geometry.winding_mixed",
                loc.clone(),
                format!(
                    "{disagree} of {judged} triangles ({:.0}%) wind against their normals ({})",
                    frac * 100.0,
                    m.name
                ),
            );
        }
    }
}

/// Weights. `validate` checks that the weights sum to 1, but a NaN slips through its comparison,
/// it only looks at the first influencing joint, and it allows negative weights. Skinning
/// multiplies by these numbers directly, so a NaN spreads to the whole vertex, a negative weight
/// pushes it the wrong way, and a joint slot past the skin reads outside the matrix palette.
fn check_weights(model: &Model, mi: usize, loc: &Location, rep: &mut Report) {
    let m = &model.meshes[mi];
    let Some(skin) = m.skin.and_then(|s| model.skins.get(s)) else {
        return;
    };
    let (mut nan, mut neg, mut oob) = (0usize, 0usize, 0usize);
    let mut first = [None, None, None];
    for (v, (w, j)) in m.weights.iter().zip(&m.joints).enumerate() {
        if w.iter().any(|c| !c.is_finite()) {
            nan += 1;
            first[0].get_or_insert(v);
            continue;
        }
        if w.iter().any(|&c| c < -1e-6) {
            neg += 1;
            first[1].get_or_insert(v);
        }
        if w.iter()
            .zip(j)
            .any(|(&w, &j)| w > 0.0 && usize::from(j) >= skin.joints.len())
        {
            oob += 1;
            first[2].get_or_insert(v);
        }
    }
    let name = &m.name;
    if nan > 0 {
        rep.push(
            Severity::Error,
            "skin.weights_non_finite",
            loc.clone(),
            format!(
                "{nan} vertices have NaN or infinite weights, the first is vertex {} ({name})",
                first[0].unwrap_or(0)
            ),
        );
    }
    if neg > 0 {
        rep.push(
            Severity::Error,
            "skin.negative_weights",
            loc.clone(),
            format!(
                "{neg} vertices have a negative weight, the first is vertex {} ({name})",
                first[1].unwrap_or(0)
            ),
        );
    }
    if oob > 0 {
        rep.push(
            Severity::Error,
            "skin.joint_out_of_range",
            loc.clone(),
            format!(
                "{oob} vertices give weight to a joint past the skin's {} joints, the first is vertex {} ({name})",
                skin.joints.len(),
                first[2].unwrap_or(0)
            ),
        );
    }
}

/// Skins. An inverse bind matrix that is not finite or not invertible collapses every vertex it
/// influences. And with the model in its node pose, `world(joint) * inverse_bind` is the identity
/// when the pose is the bind pose; if it is not, the mesh is deformed before any clip plays. That
/// is only info because some games store their bind differently (and consumers can cope by
/// applying the clip first).
pub(crate) fn check_skins(model: &Model, skin_mats: &[Vec<Mat4>], usage: &Usage, rep: &mut Report) {
    for (si, skin) in model.skins.iter().enumerate() {
        let loc = Location::skin(si);
        let (mut non_finite, mut singular, mut non_affine) = (0usize, 0usize, 0usize);
        let mut first = [None, None, None];
        let mut valid = vec![true; skin.joints.len()];
        for (k, ibm) in skin.inverse_bind.iter().enumerate() {
            if ibm.iter().any(|c| !c.is_finite()) {
                non_finite += 1;
                first[0].get_or_insert(k);
                valid[k] = false;
                continue;
            }
            let m = math::from_f32(ibm);
            if math::normalised_det3(&m).abs() < INVERSE_BIND_MIN_DET {
                singular += 1;
                first[1].get_or_insert(k);
                valid[k] = false;
            }
            if m[3].abs() > 1e-4
                || m[7].abs() > 1e-4
                || m[11].abs() > 1e-4
                || (m[15] - 1.0).abs() > 1e-4
            {
                non_affine += 1;
                first[2].get_or_insert(k);
            }
        }
        let joint_name = |k: Option<usize>| {
            k.and_then(|k| skin.joints.get(k))
                .and_then(|&j| model.nodes.get(j))
                .map_or(String::new(), |n| format!(" ({:?})", n.name))
        };
        if non_finite > 0 {
            rep.push(
                Severity::Error,
                "skin.inverse_bind_non_finite",
                loc.clone(),
                format!(
                    "{non_finite} of {} inverse bind matrices have NaN or infinite values, the first is joint {}{}",
                    skin.joints.len(),
                    first[0].unwrap_or(0),
                    joint_name(first[0])
                ),
            );
        }
        if singular > 0 {
            rep.push(
                Severity::Error,
                "skin.inverse_bind_singular",
                loc.clone(),
                format!(
                    "{singular} of {} inverse bind matrices are not invertible, the first is joint {}{}",
                    skin.joints.len(),
                    first[1].unwrap_or(0),
                    joint_name(first[1])
                ),
            );
        }
        if non_affine > 0 {
            rep.push(
                Severity::Warning,
                "skin.inverse_bind_not_affine",
                loc.clone(),
                format!(
                    "{non_affine} inverse bind matrices do not end in 0, 0, 0, 1, the first is joint {}{}",
                    first[2].unwrap_or(0),
                    joint_name(first[2])
                ),
            );
        }

        let any_used = usage.joint_used[si].iter().any(|u| *u);
        let (mut checked, mut off, mut worst, mut worst_k) = (0usize, 0usize, 0.0f64, 0usize);
        for (k, m) in skin_mats[si].iter().enumerate() {
            if !valid[k] || (any_used && !usage.joint_used[si][k]) {
                continue;
            }
            checked += 1;
            let dev = m
                .iter()
                .zip(math::IDENTITY.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f64::max);
            if dev > BIND_POSE_TOLERANCE {
                off += 1;
                if dev > worst {
                    worst = dev;
                    worst_k = k;
                }
            }
        }
        if off > 0 {
            rep.push(
                Severity::Info,
                "skin.bind_pose_mismatch",
                loc,
                format!(
                    "{off} of {checked} {}joints are not at their bind pose in the node pose \
                     (worst deviation {worst:.3} at joint {worst_k}{}); the mesh is deformed \
                     until a clip plays",
                    if any_used { "used " } else { "" },
                    joint_name(Some(worst_k))
                ),
            );
        }
    }
}
