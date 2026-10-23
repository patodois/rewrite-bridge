//! Small matrix and quaternion helpers in f64, so the checks do not depend on anything else.

use crate::{Animation, Channel, Interpolation, Node, Target};

/// Column-major 4x4, like glTF.
pub(crate) type Mat4 = [f64; 16];

pub(crate) const IDENTITY: Mat4 = [
    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
];

pub(crate) fn from_f32(m: &[f32; 16]) -> Mat4 {
    m.map(f64::from)
}

/// `T * R * S`. The quaternion is normalised first, so a slightly off one still gives a rotation.
pub(crate) fn from_trs(t: [f32; 3], q: [f32; 4], s: [f32; 3]) -> Mat4 {
    let q = q.map(f64::from);
    let len = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    let (x, y, z, w) = if len > 0.0 && len.is_finite() {
        (q[0] / len, q[1] / len, q[2] / len, q[3] / len)
    } else {
        (0.0, 0.0, 0.0, 1.0)
    };
    let s = s.map(f64::from);
    let r = [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y + z * w),
            2.0 * (x * z - y * w),
        ],
        [
            2.0 * (x * y - z * w),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z + x * w),
        ],
        [
            2.0 * (x * z + y * w),
            2.0 * (y * z - x * w),
            1.0 - 2.0 * (x * x + y * y),
        ],
    ];
    let mut m = IDENTITY;
    for (c, col) in r.iter().enumerate() {
        for (row, v) in col.iter().enumerate() {
            m[c * 4 + row] = v * s[c];
        }
    }
    m[12] = f64::from(t[0]);
    m[13] = f64::from(t[1]);
    m[14] = f64::from(t[2]);
    m
}

pub(crate) fn mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut c = [0.0; 16];
    for col in 0..4 {
        for row in 0..4 {
            c[col * 4 + row] = (0..4).map(|k| a[k * 4 + row] * b[col * 4 + k]).sum();
        }
    }
    c
}

pub(crate) fn transform_point(m: &Mat4, p: [f64; 3]) -> [f64; 3] {
    [
        m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12],
        m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13],
        m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14],
    ]
}

/// Multiplies the 3x3 part only (a direction, not a point).
pub(crate) fn transform_vector(m: &Mat4, v: [f64; 3]) -> [f64; 3] {
    [
        m[0] * v[0] + m[4] * v[1] + m[8] * v[2],
        m[1] * v[0] + m[5] * v[1] + m[9] * v[2],
        m[2] * v[0] + m[6] * v[1] + m[10] * v[2],
    ]
}

pub(crate) fn column(m: &Mat4, c: usize) -> [f64; 3] {
    [m[c * 4], m[c * 4 + 1], m[c * 4 + 2]]
}

pub(crate) fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub(crate) fn cross3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub(crate) fn len3(a: [f64; 3]) -> f64 {
    dot3(a, a).sqrt()
}

pub(crate) fn sub3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// Determinant of the upper-left 3x3 divided by the product of its column lengths: 0 for a
/// collapsed axis, ±1 for a rotation (times a mirror), whatever the scale.
pub(crate) fn normalised_det3(m: &Mat4) -> f64 {
    let (a, b, c) = (column(m, 0), column(m, 1), column(m, 2));
    let d = dot3(a, cross3(b, c));
    let den = len3(a) * len3(b) * len3(c);
    if den > 0.0 {
        d / den
    } else {
        0.0
    }
}

/// World matrices of a node list whose parents come first.
pub(crate) fn world_matrices(nodes: &[Node]) -> Vec<Mat4> {
    let locals: Vec<Mat4> = nodes
        .iter()
        .map(|n| from_trs(n.translation, n.rotation, n.scale))
        .collect();
    world_from_locals(nodes, &locals)
}

pub(crate) fn world_from_locals(nodes: &[Node], locals: &[Mat4]) -> Vec<Mat4> {
    let mut w: Vec<Mat4> = Vec::with_capacity(nodes.len());
    for (i, n) in nodes.iter().enumerate() {
        let m = match n.parent {
            Some(p) if p < i => mul(&w[p], &locals[i]),
            _ => locals[i],
        };
        w.push(m);
    }
    w
}

fn quat_dot(a: &[f64; 4], b: &[f64; 4]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3]
}

fn slerp(a: [f64; 4], mut b: [f64; 4], t: f64) -> [f64; 4] {
    let mut d = quat_dot(&a, &b);
    if d < 0.0 {
        d = -d;
        b = b.map(|c| -c);
    }
    let (wa, wb) = if d > 0.9995 {
        (1.0 - t, t)
    } else {
        let th = d.clamp(-1.0, 1.0).acos();
        let s = th.sin();
        (((1.0 - t) * th).sin() / s, (t * th).sin() / s)
    };
    let q = [
        wa * a[0] + wb * b[0],
        wa * a[1] + wb * b[1],
        wa * a[2] + wb * b[2],
        wa * a[3] + wb * b[3],
    ];
    let l = quat_dot(&q, &q).sqrt();
    if l > 0.0 {
        q.map(|c| c / l)
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

/// Offset of the keyframe's value inside `values`: cubic splines store in-tangent, value,
/// out-tangent.
pub(crate) fn value_offset(ch: &Channel, key: usize) -> usize {
    let c = ch.target.components();
    if ch.interpolation == Interpolation::CubicSpline {
        key * 3 * c + c
    } else {
        key * c
    }
}

/// The channel's value at time `t`. Cubic splines are sampled as linear between their values,
/// which is plenty for the extent checks that use this.
pub(crate) fn sample(ch: &Channel, t: f32) -> [f64; 4] {
    let c = ch.target.components();
    let get = |k: usize| -> [f64; 4] {
        let o = value_offset(ch, k);
        let mut v = [0.0; 4];
        for (i, slot) in v.iter_mut().enumerate().take(c) {
            *slot = f64::from(ch.values[o + i]);
        }
        v
    };
    let n = ch.times.len();
    if n == 0 || ch.values.len() < value_offset(ch, n - 1) + c {
        return [0.0, 0.0, 0.0, 1.0];
    }
    let hi = ch.times.partition_point(|&k| k <= t);
    if hi == 0 {
        return get(0);
    }
    if hi >= n {
        return get(n - 1);
    }
    let (a, b) = (get(hi - 1), get(hi));
    if ch.interpolation == Interpolation::Step {
        return a;
    }
    let (t0, t1) = (f64::from(ch.times[hi - 1]), f64::from(ch.times[hi]));
    let u = ((f64::from(t) - t0) / (t1 - t0)).clamp(0.0, 1.0);
    if ch.target == Target::Rotation {
        slerp(a, b, u)
    } else {
        [
            a[0] + (b[0] - a[0]) * u,
            a[1] + (b[1] - a[1]) * u,
            a[2] + (b[2] - a[2]) * u,
            0.0,
        ]
    }
}

/// World matrices of the nodes with the clip applied at time `t`.
pub(crate) fn posed_worlds(nodes: &[Node], anim: &Animation, t: f32) -> Vec<Mat4> {
    let mut trs: Vec<([f32; 3], [f32; 4], [f32; 3])> = nodes
        .iter()
        .map(|n| (n.translation, n.rotation, n.scale))
        .collect();
    for ch in &anim.channels {
        let Some(slot) = trs.get_mut(ch.node) else {
            continue;
        };
        let v = sample(ch, t).map(|c| c as f32);
        match ch.target {
            Target::Translation => slot.0 = [v[0], v[1], v[2]],
            Target::Rotation => slot.1 = v,
            Target::Scale => slot.2 = [v[0], v[1], v[2]],
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }
    let locals: Vec<Mat4> = trs.iter().map(|&(t, q, s)| from_trs(t, q, s)).collect();
    world_from_locals(nodes, &locals)
}
