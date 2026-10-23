//! Reference CPU code for depth-aware frames (contract section 9.7): converting depth buffers to
//! the linear metres a pixel file carries, reprojecting a guest frame from the camera it was
//! rendered with to the host's current camera, and compositing it against the host's own image
//! with a depth test.
//!
//! This is a reference for testing and for small images, written for clarity and checked against
//! hand-computed numbers. An engine does the same three steps on the GPU (a depth-to-linear
//! shader, a point or mesh warp into a depth-tested target, and a depth-aware blit); nothing here
//! is fast enough for a 1080p frame at 60 Hz.
//!
//! Conventions, all from the contract: a camera looks down its local -Z; "linear depth" is the
//! distance along that axis in metres, positive in front of the camera (`-z` in view space);
//! pixel `(0, 0)` is the top-left corner, and normalized device coordinates have +x to the right
//! and +y up, whatever the host's graphics API does to its framebuffer.

use std::collections::VecDeque;

use super::{Camera, DepthRange, WireError};

// ---------------------------------------------------------------------------------------------
// Depth conversion

/// Converts a depth-buffer value (or clip-space `z / w`) of a perspective projection with the
/// given `near`, `far` and `range` into linear view depth in metres. `far` may be
/// `f32::INFINITY`.
///
/// The input is the value in the convention of `range` (contract section 9.3): `-1..=1` for
/// [`DepthRange::NegOneToOne`] (NDC `z`; an OpenGL depth buffer holds `(z + 1) / 2`, so pass
/// `2 * d - 1`), `0..=1` for the other two. A value that maps to nothing in front of the camera,
/// or a NaN, gives `f32::INFINITY` ("nothing here").
pub fn linearize_depth(d: f32, near: f32, far: f32, range: DepthRange) -> f32 {
    let (d, n, f) = (f64::from(d), f64::from(near), f64::from(far));
    let finite = far.is_finite();
    let t = match range {
        DepthRange::NegOneToOne if finite => 2.0 * f * n / ((f + n) - d * (f - n)),
        DepthRange::NegOneToOne => 2.0 * n / (1.0 - d),
        DepthRange::ZeroToOne if finite => f * n / (f - d * (f - n)),
        DepthRange::ZeroToOne => n / (1.0 - d),
        DepthRange::Reversed if finite => n * f / (n + d * (f - n)),
        DepthRange::Reversed => n / d,
    };
    if t.is_finite() && t > 0.0 {
        t as f32
    } else {
        f32::INFINITY
    }
}

/// The inverse of [`linearize_depth`]: the depth-buffer value a surface `t` metres in front of
/// the camera gets. A `t` that is not a finite positive number gives the value a cleared buffer
/// holds (`1` for the first two conventions, `0` for the reversed one).
pub fn delinearize_depth(t: f32, near: f32, far: f32, range: DepthRange) -> f32 {
    if !(t.is_finite() && t > 0.0) {
        return match range {
            DepthRange::Reversed => 0.0,
            _ => 1.0,
        };
    }
    let (t, n, f) = (f64::from(t), f64::from(near), f64::from(far));
    let finite = far.is_finite();
    let d = match range {
        DepthRange::NegOneToOne if finite => (f + n) / (f - n) - 2.0 * f * n / ((f - n) * t),
        DepthRange::NegOneToOne => 1.0 - 2.0 * n / t,
        DepthRange::ZeroToOne if finite => f * (t - n) / (t * (f - n)),
        DepthRange::ZeroToOne => 1.0 - n / t,
        DepthRange::Reversed if finite => n * (f - t) / (t * (f - n)),
        DepthRange::Reversed => n / t,
    };
    d as f32
}

/// Converts a whole depth buffer, in the convention of `camera.depth_range`, to linear metres
/// using the camera's `near` and `far`.
pub fn linearize_buffer(depth: &[f32], camera: &Camera) -> Vec<f32> {
    depth
        .iter()
        .map(|&d| linearize_depth(d, camera.near, camera.far, camera.depth_range))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// 4x4 matrices in f64, row-major here (the wire is column-major f32)

type Mat = [[f64; 4]; 4];

fn mat(m: &[f32; 16]) -> Mat {
    std::array::from_fn(|r| std::array::from_fn(|c| f64::from(m[c * 4 + r])))
}

fn mul(a: &Mat, b: &Mat) -> Mat {
    std::array::from_fn(|r| std::array::from_fn(|c| (0..4).map(|k| a[r][k] * b[k][c]).sum::<f64>()))
}

fn apply(m: &Mat, v: [f64; 4]) -> [f64; 4] {
    std::array::from_fn(|r| (0..4).map(|c| m[r][c] * v[c]).sum::<f64>())
}

/// Gauss-Jordan inverse with partial pivoting; `None` for a singular matrix.
fn inverse(m: &Mat) -> Option<Mat> {
    let mut a = *m;
    let mut inv: Mat = std::array::from_fn(|r| std::array::from_fn(|c| f64::from(r == c)));
    for col in 0..4 {
        let pivot = (col..4).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        let p = a[pivot][col].abs();
        if p.is_nan() || p <= 1e-300 {
            return None;
        }
        a.swap(col, pivot);
        inv.swap(col, pivot);
        let p = a[col][col];
        for c in 0..4 {
            a[col][c] /= p;
            inv[col][c] /= p;
        }
        for r in 0..4 {
            if r != col {
                let f = a[r][col];
                for c in 0..4 {
                    a[r][c] -= f * a[col][c];
                    inv[r][c] -= f * inv[col][c];
                }
            }
        }
    }
    Some(inv)
}

/// Everything about a camera the warp needs, computed once.
struct Rig {
    view: Mat,
    view_inv: Mat,
    proj: Mat,
}

impl Rig {
    fn new(c: &Camera) -> Result<Self, WireError> {
        let view = mat(&c.view);
        Ok(Self {
            view_inv: inverse(&view).ok_or(WireError::BadValue("singular view matrix"))?,
            view,
            proj: mat(&c.proj),
        })
    }

    /// The view-space point `(x, y, -t)` that projects to `(nx, ny)` in NDC, for a surface `t`
    /// metres in front of the camera. Solves the two projection rows for `x` and `y`, so it holds
    /// for any perspective or orthographic matrix, centred or not.
    fn unproject(&self, nx: f64, ny: f64, t: f64) -> Option<[f64; 4]> {
        let p = &self.proj;
        let z = -t;
        // n * w_clip = clip_n, with w_clip and clip_n affine in (x, y) once z is fixed.
        let eq = |n: f64, row: &[f64; 4]| {
            let (a, b) = (row[0] - n * p[3][0], row[1] - n * p[3][1]);
            let rhs = n * (p[3][2] * z + p[3][3]) - (row[2] * z + row[3]);
            (a, b, rhs)
        };
        let (a1, b1, r1) = eq(nx, &p[0]);
        let (a2, b2, r2) = eq(ny, &p[1]);
        let det = a1 * b2 - a2 * b1;
        if det.is_nan() || det.abs() <= 1e-12 {
            return None;
        }
        Some([(r1 * b2 - r2 * b1) / det, (a1 * r2 - a2 * r1) / det, z, 1.0])
    }
}

/// Maps points from the render camera's view space to the present camera's window.
struct Warp {
    /// Render view space to present view space.
    rel: Mat,
    /// Render view space to present clip space.
    clip: Mat,
}

impl Warp {
    fn new(render: &Rig, present: &Rig) -> Self {
        let rel = mul(&present.view, &render.view_inv);
        Self {
            clip: mul(&present.proj, &rel),
            rel,
        }
    }

    /// Window position in pixels and linear depth of a render-view-space point, `None` when it is
    /// not in front of the present camera.
    fn project(&self, p: [f64; 4], w: f64, h: f64) -> Option<(f64, f64, f64)> {
        let t = -(0..4).map(|c| self.rel[2][c] * p[c]).sum::<f64>();
        let c = apply(&self.clip, p);
        if !(t > 1e-9 && c[3] > 1e-12) {
            return None;
        }
        Some((
            (c[0] / c[3] * 0.5 + 0.5) * w,
            (0.5 - c[1] / c[3] * 0.5) * h,
            t,
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// Reprojection

/// Widest footprint, in destination pixels, one source pixel may cover. Closer than that the
/// splat falls back to a single pixel rather than smearing across the frame.
const MAX_FOOTPRINT: f64 = 16.0;

/// Reprojects a guest frame, rendered with `render_cam`, to the view of `present_cam`.
///
/// `src_rgba` is `w * h` straight-alpha RGBA pixels and `src_depth` their linear depth in metres
/// (contract section 9.6); pixels with alpha 0 or a non-finite depth are empty and skipped. Each
/// remaining pixel is unprojected with the render camera at its depth, moved to the present
/// camera, and splatted over the destination pixels its square footprint covers (at least one),
/// nearest surface winning. The footprint's corners are warped at the average depth of the pixels
/// that share them, so a smooth surface stays closed. The result is `dst_w * dst_h` RGBA pixels
/// and their linear depth as seen from `present_cam`; **holes** (what the guest never saw,
/// disocclusions, and the area outside the old view) stay transparent with depth `+inf`.
///
/// With the same camera for both and `dst` = `src` size the frame comes back unchanged. With a
/// different size it is resampled, which is how a receiver can also scale a frame it could not
/// match to a camera (render camera = present camera).
///
/// This is a reference for testing and small images: splats can still leave cracks where a surface
/// is stretched a lot (very close to the camera, or seen almost edge-on), and shading that depends on the view (specular, reflections) is not
/// corrected. Engines do this on the GPU, usually as a mesh warp.
// The eight parameters are the signature the design asked for: image and depth with their size,
// the two cameras, and the destination size.
#[allow(clippy::too_many_arguments)]
pub fn reproject(
    src_rgba: &[u8],
    src_depth: &[f32],
    w: u32,
    h: u32,
    render_cam: &Camera,
    present_cam: &Camera,
    dst_w: u32,
    dst_h: u32,
) -> Result<(Vec<u8>, Vec<f32>), WireError> {
    let n = (w as usize)
        .checked_mul(h as usize)
        .ok_or(WireError::BadValue("source size overflows"))?;
    let dst_n = (dst_w as usize)
        .checked_mul(dst_h as usize)
        .filter(|&d| d <= 1 << 28)
        .ok_or(WireError::BadValue("destination too large"))?;
    if src_depth.len() != n || src_rgba.len() != n * 4 {
        return Err(WireError::BadValue("source is not w * h pixels with depth"));
    }
    let mut out = vec![0u8; dst_n * 4];
    let mut out_depth = vec![f32::INFINITY; dst_n];
    if n == 0 || dst_n == 0 {
        return Ok((out, out_depth));
    }
    let render = Rig::new(render_cam)?;
    let present = Rig::new(present_cam)?;
    let warp_to_present = Warp::new(&render, &present);
    let (fw, fh) = (f64::from(w), f64::from(h));
    let (dw, dh) = (f64::from(dst_w), f64::from(dst_h));
    let to_ndc = |px: f64, py: f64| (px / fw * 2.0 - 1.0, 1.0 - py / fh * 2.0);
    // A source pixel-grid point at depth `t`, in destination window coordinates.
    let warp = |px: f64, py: f64, t: f64| -> Option<(f64, f64, f64)> {
        let (nx, ny) = to_ndc(px, py);
        let view_pt = render.unproject(nx, ny, t)?;
        warp_to_present.project(view_pt, dw, dh)
    };

    // The depth a pixel-grid corner is warped at: the average over the pixels around it whose
    // depth is within 10% of `t`, so neighbouring squares of a smooth surface share their corners
    // and tile without cracks, while a silhouette keeps each side at its own depth.
    let corner_depth = |cx: usize, cy: usize, t: f64| -> f64 {
        let (mut sum, mut n) = (0.0, 0.0);
        for y in cy.saturating_sub(1)..=cy.min(h as usize - 1) {
            for x in cx.saturating_sub(1)..=cx.min(w as usize - 1) {
                let d = f64::from(src_depth[y * w as usize + x]);
                if src_rgba[(y * w as usize + x) * 4 + 3] != 0
                    && d.is_finite()
                    && (d - t).abs() <= 0.1 * t
                {
                    sum += d;
                    n += 1.0;
                }
            }
        }
        sum / n
    };

    for y in 0..h as usize {
        for x in 0..w as usize {
            let i = y * w as usize + x;
            let t = f64::from(src_depth[i]);
            if src_rgba[i * 4 + 3] == 0 || !t.is_finite() || t <= 0.0 {
                continue;
            }
            let (px, py) = (x as f64, y as f64);
            let Some((cx, cy, depth)) = warp(px + 0.5, py + 0.5, t) else {
                continue;
            };
            // The pixel's square, warped: every destination pixel centre inside its bounding
            // box. Falls back to the pixel holding the centre if the box is degenerate.
            let corners = [(0, 0), (1, 0), (0, 1), (1, 1)].map(|(dx, dy)| {
                warp(
                    px + dx as f64,
                    py + dy as f64,
                    corner_depth(x + dx, y + dy, t),
                )
            });
            let (mut x0, mut x1, mut y0, mut y1) = (cx.floor(), cx.floor(), cy.floor(), cy.floor());
            if let [Some(a), Some(b), Some(c), Some(d)] = corners {
                let xs = [a.0, b.0, c.0, d.0];
                let ys = [a.1, b.1, c.1, d.1];
                let (minx, maxx) = (min(&xs), max(&xs));
                let (miny, maxy) = (min(&ys), max(&ys));
                let (fx0, fx1) = ((minx - 0.5).ceil(), (maxx - 0.5).floor());
                let (fy0, fy1) = ((miny - 0.5).ceil(), (maxy - 0.5).floor());
                if fx0 <= fx1
                    && fy0 <= fy1
                    && maxx - minx <= MAX_FOOTPRINT
                    && maxy - miny <= MAX_FOOTPRINT
                {
                    (x0, x1, y0, y1) = (fx0, fx1, fy0, fy1);
                }
            }
            let (x0, x1) = (x0.max(0.0), x1.min(dw - 1.0));
            let (y0, y1) = (y0.max(0.0), y1.min(dh - 1.0));
            if x0 > x1 || y0 > y1 {
                continue;
            }
            for dy in y0 as usize..=y1 as usize {
                for dx in x0 as usize..=x1 as usize {
                    let j = dy * dst_w as usize + dx;
                    if (depth as f32) < out_depth[j] {
                        out_depth[j] = depth as f32;
                        out[j * 4..j * 4 + 4].copy_from_slice(&src_rgba[i * 4..i * 4 + 4]);
                    }
                }
            }
        }
    }
    Ok((out, out_depth))
}

fn min(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::INFINITY, f64::min)
}

fn max(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}

// ---------------------------------------------------------------------------------------------
// Compositing

/// The result of [`composite`].
#[derive(Clone, Debug, PartialEq)]
pub struct Composited {
    /// The blended image, straight alpha.
    pub rgba: Vec<u8>,
    /// Linear depth of whatever is nearest at each pixel (the guest's where it won).
    pub depth: Vec<f32>,
    /// Guest pixels that were nearer than the host's and so covered it.
    pub guest_in_front: usize,
    /// Guest pixels that were drawn behind the host's scene and so hidden.
    pub guest_hidden: usize,
}

/// Composites a guest frame over the host's image with a depth test.
///
/// All four slices cover the same `n` pixels (`*_rgba` has `4 * n` bytes); both depths are linear
/// metres (convert a host depth buffer with [`linearize_buffer`]). Where the guest pixel is not
/// empty (alpha above 0, finite positive depth) and strictly nearer than the host's (`+inf` or
/// NaN there means the host drew nothing), it is blended over the host with straight-alpha
/// "over", in the encoded (sRGB) values as the pixel file carries them; otherwise the host pixel
/// stays. Equal depths go to the host.
pub fn composite(
    host_rgba: &[u8],
    host_depth: &[f32],
    guest_rgba: &[u8],
    guest_depth: &[f32],
) -> Result<Composited, WireError> {
    let n = host_depth.len();
    if host_rgba.len() != n * 4 || guest_rgba.len() != n * 4 || guest_depth.len() != n {
        return Err(WireError::BadValue("images and depths differ in size"));
    }
    let mut rgba = host_rgba.to_vec();
    let mut depth = host_depth.to_vec();
    let (mut in_front, mut hidden) = (0, 0);
    for i in 0..n {
        let (g, gd) = (&guest_rgba[i * 4..i * 4 + 4], guest_depth[i]);
        if g[3] == 0 || !gd.is_finite() || gd <= 0.0 {
            continue;
        }
        let hd = if host_depth[i].is_nan() {
            f32::INFINITY
        } else {
            host_depth[i]
        };
        if gd >= hd {
            hidden += 1;
            continue;
        }
        in_front += 1;
        let a = u32::from(g[3]);
        let px = &mut rgba[i * 4..i * 4 + 4];
        for c in 0..3 {
            px[c] = ((u32::from(g[c]) * a + u32::from(px[c]) * (255 - a) + 127) / 255) as u8;
        }
        px[3] = (a + (u32::from(px[3]) * (255 - a) + 127) / 255).min(255) as u8;
        depth[i] = gd;
    }
    Ok(Composited {
        rgba,
        depth,
        guest_in_front: in_front,
        guest_hidden: hidden,
    })
}

// ---------------------------------------------------------------------------------------------
// Camera ring

/// The host's last few [`Camera`]s by datagram `seq`, so a frame's `camera_seq` can be turned back
/// into the camera it was rendered with.
#[derive(Clone, Debug)]
pub struct CameraRing {
    cap: usize,
    items: VecDeque<(u32, Camera)>,
}

impl CameraRing {
    /// Half a second of cameras at 60 Hz, the window the contract suggests.
    pub const DEFAULT_CAPACITY: usize = 32;

    pub fn new(capacity: usize) -> Self {
        Self {
            cap: capacity.max(1),
            items: VecDeque::new(),
        }
    }

    /// Remembers the camera sent in the datagram numbered `seq`, forgetting the oldest beyond the
    /// capacity.
    pub fn push(&mut self, seq: u32, camera: Camera) {
        self.items.retain(|(s, _)| *s != seq);
        if self.items.len() == self.cap {
            self.items.pop_front();
        }
        self.items.push_back((seq, camera));
    }

    /// The camera sent with `seq`, or `None` for 0 ("none"), a forgotten seq or one never sent.
    pub fn get(&self, seq: u32) -> Option<&Camera> {
        if seq == 0 {
            return None;
        }
        self.items.iter().find(|(s, _)| *s == seq).map(|(_, c)| c)
    }

    /// The newest camera pushed.
    pub fn latest(&self) -> Option<&Camera> {
        self.items.back().map(|(_, c)| c)
    }
}

impl Default for CameraRing {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RANGES: [DepthRange; 3] = [
        DepthRange::NegOneToOne,
        DepthRange::ZeroToOne,
        DepthRange::Reversed,
    ];

    fn cam(eye: [f32; 3], target: [f32; 3]) -> Camera {
        // fov_y 90 degrees, square viewport: ndc = view / depth.
        Camera::look_at(
            eye,
            target,
            std::f32::consts::FRAC_PI_2,
            0.1,
            100.0,
            [100, 100],
        )
    }

    fn rel(a: f32, b: f32) -> f32 {
        (a - b).abs() / b.abs().max(1e-6)
    }

    #[test]
    fn linearize_round_trips_in_every_convention() {
        for range in RANGES {
            for far in [100.0, 1000.0, f32::INFINITY] {
                for t in [0.1f32, 0.11, 0.5, 1.0, 3.0, 10.0, 50.0, 99.0] {
                    if far.is_finite() && t > far {
                        continue;
                    }
                    let d = delinearize_depth(t, 0.1, far, range);
                    let back = linearize_depth(d, 0.1, far, range);
                    // f32 depth values near 1 resolve less well far away; ZeroToOne and GL
                    // lose a few parts per ten thousand by 100 m, reversed does not.
                    assert!(
                        rel(back, t) < 2e-3,
                        "{range:?} far {far}: {t} -> {d} -> {back}"
                    );
                }
            }
        }
    }

    #[test]
    fn linearize_hits_the_plane_values() {
        let near = 0.25;
        for (far, planes) in [(40.0f32, true), (f32::INFINITY, false)] {
            // (range, value at near, value at far)
            let table = [
                (DepthRange::NegOneToOne, -1.0, 1.0),
                (DepthRange::ZeroToOne, 0.0, 1.0),
                (DepthRange::Reversed, 1.0, 0.0),
            ];
            for (range, at_near, at_far) in table {
                assert!(rel(linearize_depth(at_near, near, far, range), near) < 1e-5);
                assert!((delinearize_depth(near, near, far, range) - at_near).abs() < 1e-5);
                if planes {
                    assert!(rel(linearize_depth(at_far, near, far, range), far) < 1e-5);
                } else {
                    // An infinite far plane is reached only at infinity.
                    assert_eq!(linearize_depth(at_far, near, far, range), f32::INFINITY);
                }
            }
        }
        // Not drawn / not in front of the camera.
        for range in RANGES {
            assert_eq!(linearize_depth(f32::NAN, 0.1, 100.0, range), f32::INFINITY);
        }
        assert_eq!(
            delinearize_depth(f32::INFINITY, 0.1, 100.0, DepthRange::Reversed),
            0.0
        );
        assert_eq!(
            delinearize_depth(f32::INFINITY, 0.1, 100.0, DepthRange::ZeroToOne),
            1.0
        );
        assert_eq!(
            delinearize_depth(-1.0, 0.1, 100.0, DepthRange::NegOneToOne),
            1.0
        );
    }

    #[test]
    fn projection_matrices_agree_with_the_depth_formulas() {
        // Project a point t metres ahead with each convention's matrix: z_clip / w_clip is the
        // value delinearize_depth gives, so the contract's formulas and Camera::with_depth_range
        // describe the same projection.
        for range in RANGES {
            for far in [100.0, f32::INFINITY] {
                let c = Camera::look_at([0.0; 3], [0.0, 0.0, -1.0], 1.0, 0.1, far, [64, 48])
                    .with_depth_range(range);
                assert_eq!(c.depth_range, range);
                for t in [0.1f32, 1.0, 7.5, 60.0] {
                    let p = mat(&c.proj);
                    let clip = apply(&p, [0.0, 0.0, -f64::from(t), 1.0]);
                    let ndc_z = (clip[2] / clip[3]) as f32;
                    let d = delinearize_depth(t, 0.1, far, range);
                    assert!(
                        (ndc_z - d).abs() < 2e-5,
                        "{range:?} far {far} t {t}: {ndc_z} vs {d}"
                    );
                }
            }
        }
    }

    #[test]
    fn matrix_inverse_is_an_inverse() {
        let c = cam([1.0, 2.0, 3.0], [-4.0, 0.5, -2.0]);
        let v = mat(&c.view);
        let i = mul(&v, &inverse(&v).unwrap());
        for (r, row) in i.iter().enumerate() {
            for (k, x) in row.iter().enumerate() {
                assert!((x - f64::from(r == k)).abs() < 1e-9);
            }
        }
        assert!(inverse(&[[0.0; 4]; 4]).is_none());
    }

    /// Opaque image whose colour tells where each pixel came from: R = x, G = y.
    fn grid(w: u32, h: u32, depth: impl Fn(u32, u32) -> f32) -> (Vec<u8>, Vec<f32>) {
        let mut rgba = Vec::new();
        let mut d = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[x as u8, y as u8, 7, 255]);
                d.push(depth(x, y));
            }
        }
        (rgba, d)
    }

    #[test]
    fn same_camera_reproduces_the_image() {
        let c = cam([1.0, 2.0, 3.0], [0.0, 1.0, -4.0]);
        let (w, h) = (64u32, 48u32);
        let (mut rgba, mut depth) = grid(w, h, |x, y| 1.5 + 0.1 * x as f32 + 0.37 * (y % 7) as f32);
        // Some empty pixels, one by alpha and one by depth.
        rgba[(5 * w as usize + 9) * 4 + 3] = 0;
        depth[7 * w as usize + 3] = f32::INFINITY;
        let (out, out_depth) = reproject(&rgba, &depth, w, h, &c, &c, w, h).unwrap();
        for i in 0..(w * h) as usize {
            let empty = rgba[i * 4 + 3] == 0 || !depth[i].is_finite();
            if empty {
                assert_eq!(&out[i * 4..i * 4 + 4], &[0, 0, 0, 0]);
                assert_eq!(out_depth[i], f32::INFINITY);
            } else {
                assert_eq!(&out[i * 4..i * 4 + 4], &rgba[i * 4..i * 4 + 4], "pixel {i}");
                assert!(rel(out_depth[i], depth[i]) < 1e-4, "depth {i}");
            }
        }
    }

    #[test]
    fn a_larger_target_scales_the_image_up() {
        let c = cam([0.0; 3], [0.0, 0.0, -1.0]);
        let (rgba, depth) = grid(20, 10, |_, _| 4.0);
        let (out, _) = reproject(&rgba, &depth, 20, 10, &c, &c, 40, 20).unwrap();
        for y in 0..20usize {
            for x in 0..40usize {
                let s = (y / 2) * 20 + x / 2;
                assert_eq!(
                    &out[(y * 40 + x) * 4..][..4],
                    &rgba[s * 4..][..4],
                    "({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn a_point_shifts_by_the_parallax_a_sideways_move_causes() {
        // Pixel (49, 50) of 100 x 100 sits 4 m away at ndc (-0.01, -0.01 ...). Moving the camera
        // 1 m to the right (+x) moves it 1 m to the left in view space. With a 90 degree field
        // of view, 50 px per unit of x / depth: -1 m * 50 px / 4 m = -12.5 px.
        let a = cam([0.0, 0.0, 0.0], [0.0, 0.0, -1.0]);
        let b = cam([1.0, 0.0, 0.0], [1.0, 0.0, -1.0]);
        let mut rgba = vec![0u8; 100 * 100 * 4];
        let mut depth = vec![f32::INFINITY; 100 * 100];
        let i = 50 * 100 + 49;
        rgba[i * 4..i * 4 + 4].copy_from_slice(&[200, 100, 50, 255]);
        depth[i] = 4.0;
        let (out, out_depth) = reproject(&rgba, &depth, 100, 100, &a, &b, 100, 100).unwrap();
        let hits: Vec<usize> = (0..10_000).filter(|&j| out[j * 4 + 3] != 0).collect();
        assert!(!hits.is_empty() && hits.len() <= 2, "{hits:?}");
        let cx: f64 = hits.iter().map(|j| (j % 100) as f64 + 0.5).sum::<f64>() / hits.len() as f64;
        let cy: f64 = hits.iter().map(|j| (j / 100) as f64 + 0.5).sum::<f64>() / hits.len() as f64;
        assert!((cx - (49.5 - 12.5)).abs() <= 0.5, "x {cx}");
        assert!((cy - 50.5).abs() <= 0.5, "y {cy}");
        assert!(rel(out_depth[hits[0]], 4.0) < 1e-5, "depth stays 4 m");
        assert_eq!(&out[hits[0] * 4..][..4], &[200, 100, 50, 255]);
    }

    #[test]
    fn a_wall_moves_as_one_when_the_camera_slides() {
        let a = cam([0.0, 0.0, 0.0], [0.0, 0.0, -1.0]);
        let b = cam([1.0, 0.0, 0.0], [1.0, 0.0, -1.0]);
        let (rgba, depth) = grid(100, 100, |_, _| 4.0);
        let (out, out_depth) = reproject(&rgba, &depth, 100, 100, &a, &b, 100, 100).unwrap();
        for y in [10usize, 50, 90] {
            for x in 0..87usize {
                // Destination pixel x shows the source pixel 12.5 to its right.
                let j = y * 100 + x;
                let src_x = f64::from(out[j * 4]);
                assert!(
                    (src_x - (x as f64 + 12.5)).abs() <= 0.5,
                    "({x}, {y}) shows {src_x}"
                );
                assert_eq!(out[j * 4 + 1], y as u8);
                assert!(rel(out_depth[j], 4.0) < 1e-5);
            }
            // The strip the old view never saw is a hole.
            for x in 88..100usize {
                assert_eq!(out[(y * 100 + x) * 4 + 3], 0, "({x}, {y})");
                assert_eq!(out_depth[y * 100 + x], f32::INFINITY);
            }
        }
    }

    #[test]
    fn a_pure_rotation_shifts_every_depth_alike() {
        // Turn the camera 10 degrees to the left in place: a point straight ahead appears
        // tan(10) / tan(fov / 2) of the half-width to the right, at any depth.
        let theta = 10f64.to_radians();
        let a = cam([0.0; 3], [0.0, 0.0, -1.0]);
        let b = cam([0.0; 3], [-theta.sin() as f32, 0.0, -theta.cos() as f32]);
        for t in [2.0f32, 5.0, 40.0] {
            let (px, py) = (49usize, 30usize);
            let mut rgba = vec![0u8; 100 * 100 * 4];
            let mut depth = vec![f32::INFINITY; 100 * 100];
            rgba[(py * 100 + px) * 4..][..4].copy_from_slice(&[9, 9, 9, 255]);
            depth[py * 100 + px] = t;
            let (out, out_depth) = reproject(&rgba, &depth, 100, 100, &a, &b, 100, 100).unwrap();
            let hits: Vec<usize> = (0..10_000).filter(|&j| out[j * 4 + 3] != 0).collect();
            assert!(!hits.is_empty() && hits.len() <= 4, "t {t}: {hits:?}");

            // The same thing worked out by hand: world point, then the new camera's basis.
            let (nx, ny) = (
                (px as f64 + 0.5) / 50.0 - 1.0,
                1.0 - (py as f64 + 0.5) / 50.0,
            );
            let p = [nx * f64::from(t), ny * f64::from(t), -f64::from(t)];
            let f = [-theta.sin(), 0.0, -theta.cos()];
            let s = [theta.cos(), 0.0, -theta.sin()]; // f x up
            let u = [0.0, 1.0, 0.0];
            let dot = |a: [f64; 3], b: [f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
            let depth_b = dot(p, f);
            let ex = (dot(p, s) / depth_b + 1.0) * 50.0;
            let ey = (1.0 - dot(p, u) / depth_b) * 50.0;
            let n = hits.len() as f64;
            let cx = hits.iter().map(|j| (j % 100) as f64 + 0.5).sum::<f64>() / n;
            let cy = hits.iter().map(|j| (j / 100) as f64 + 0.5).sum::<f64>() / n;
            assert!((cx - ex).abs() <= 0.5, "t {t}: x {cx} vs {ex}");
            assert!((cy - ey).abs() <= 0.5, "t {t}: y {cy} vs {ey}");
            assert!(
                (ex - 49.5 - 0.1763 * 50.0).abs() < 0.2,
                "the expected shift is ~8.8 px"
            );
            assert!(rel(out_depth[hits[0]], depth_b as f32) < 1e-5);
        }
    }

    #[test]
    fn a_stretched_slanted_surface_stays_closed() {
        // A plane tilted away to the right, seen again from 1.5 m further forward and a bit
        // aside: every source pixel grows to over a pixel and a half, and none may leave a crack
        // inside the surface.
        let a = cam([0.0; 3], [0.0, 0.0, -1.0]);
        let b = cam([0.3, 0.1, -1.5], [0.3, 0.1, -2.5]);
        let (rgba, depth) = grid(100, 100, |x, _| 3.0 + 0.04 * x as f32);
        let (out, _) = reproject(&rgba, &depth, 100, 100, &a, &b, 100, 100).unwrap();
        let mut spans = 0;
        for y in 30..70usize {
            let row: Vec<bool> = (0..100).map(|x| out[(y * 100 + x) * 4 + 3] != 0).collect();
            let first = row.iter().position(|&f| f).unwrap();
            let last = row.iter().rposition(|&f| f).unwrap();
            assert!(last - first > 50, "row {y}: {first}..{last}");
            assert!(row[first..=last].iter().all(|&f| f), "a crack in row {y}");
            spans += 1;
        }
        assert_eq!(spans, 40);
    }

    #[test]
    fn nearest_surface_wins_when_pixels_collide() {
        // Camera b is 0.5 m to the right of a, so a pixel d metres away moves 25 / d px to the
        // left. A red pixel (column 60, 4 m) lands at 54.25; so does a blue one (column 70) at
        // 25 / (70.5 - 54.25) m, which is nearer, so blue must be what is left there.
        let a = cam([0.0; 3], [0.0, 0.0, -1.0]);
        let b = cam([0.5, 0.0, 0.0], [0.5, 0.0, -1.0]);
        let blue_depth = 25.0 / (70.5 - 54.25);
        for blue_first in [false, true] {
            let mut rgba = vec![0u8; 100 * 100 * 4];
            let mut depth = vec![f32::INFINITY; 100 * 100];
            let put = |rgba: &mut Vec<u8>, depth: &mut Vec<f32>, x: usize, c: [u8; 4], d: f32| {
                rgba[(50 * 100 + x) * 4..][..4].copy_from_slice(&c);
                depth[50 * 100 + x] = d;
            };
            // Scan order decides which is splatted first; the result must not depend on it.
            let (red_x, blue_x) = if blue_first { (70, 60) } else { (60, 70) };
            let (red_d, blue_d) = if blue_first {
                (25.0 / (70.5 - 54.25), 4.0)
            } else {
                (4.0, blue_depth)
            };
            put(&mut rgba, &mut depth, red_x, [255, 0, 0, 255], red_d);
            put(&mut rgba, &mut depth, blue_x, [0, 0, 255, 255], blue_d);
            let (out, out_depth) = reproject(&rgba, &depth, 100, 100, &a, &b, 100, 100).unwrap();
            let hits: Vec<usize> = (0..10_000).filter(|&j| out[j * 4 + 3] != 0).collect();
            assert_eq!(hits, vec![50 * 100 + 54], "blue_first {blue_first}");
            let near = out[hits[0] * 4..][..4].to_vec();
            let want = if blue_first {
                [255, 0, 0, 255]
            } else {
                [0, 0, 255, 255]
            };
            assert_eq!(
                near, want,
                "the nearer surface is in front (blue_first {blue_first})"
            );
            assert!(out_depth[hits[0]] < 1.6);
        }
    }

    #[test]
    fn bad_sizes_are_errors_not_panics() {
        let c = cam([0.0; 3], [0.0, 0.0, -1.0]);
        assert!(reproject(&[0; 3], &[0.0], 1, 1, &c, &c, 1, 1).is_err());
        assert!(reproject(&[0; 4], &[0.0; 2], 1, 1, &c, &c, 1, 1).is_err());
        assert!(reproject(&[0; 4], &[1.0], 1, 1, &c, &c, u32::MAX, u32::MAX).is_err());
        assert!(composite(&[0; 8], &[0.0; 2], &[0; 4], &[0.0; 1]).is_err());
        let mut broken = c;
        broken.view = [0.0; 16];
        assert!(reproject(&[0; 4], &[1.0], 1, 1, &broken, &c, 1, 1).is_err());
        // Garbage depths and cameras may produce holes but not panics.
        let mut weird = c;
        weird.proj = [f32::NAN; 16];
        let _ = reproject(&[9; 4], &[1.0], 1, 1, &c, &weird, 1, 1);
        let _ = reproject(&[9; 4], &[f32::NAN], 1, 1, &c, &c, 1, 1);
    }

    // ----- Compositing

    const N: usize = 16;

    fn flat(rgba: [u8; 4], depth: f32) -> (Vec<u8>, Vec<f32>) {
        (rgba.repeat(N * N), vec![depth; N * N])
    }

    /// A guest image with an opaque quad on rows 4..12 and columns 4..12 at `depth`.
    fn quad(depth: f32) -> (Vec<u8>, Vec<f32>) {
        let (mut rgba, mut d) = (vec![0u8; N * N * 4], vec![f32::INFINITY; N * N]);
        for y in 4..12 {
            for x in 4..12 {
                rgba[(y * N + x) * 4..][..4].copy_from_slice(&[0, 0, 255, 255]);
                d[y * N + x] = depth;
            }
        }
        (rgba, d)
    }

    #[test]
    fn a_host_wall_at_5_m_hides_a_guest_quad_at_8_m() {
        let (host, host_depth) = flat([255, 0, 0, 255], 5.0);
        let (guest, guest_depth) = quad(8.0);
        let out = composite(&host, &host_depth, &guest, &guest_depth).unwrap();
        assert_eq!(out.rgba, host, "the wall is untouched");
        assert_eq!(out.depth, host_depth);
        assert_eq!((out.guest_in_front, out.guest_hidden), (0, 64));
    }

    #[test]
    fn a_guest_quad_at_3_m_covers_the_host_wall() {
        let (host, host_depth) = flat([255, 0, 0, 255], 5.0);
        let (guest, guest_depth) = quad(3.0);
        let out = composite(&host, &host_depth, &guest, &guest_depth).unwrap();
        for y in 0..N {
            for x in 0..N {
                let inside = (4..12).contains(&x) && (4..12).contains(&y);
                let want: [u8; 4] = if inside {
                    [0, 0, 255, 255]
                } else {
                    [255, 0, 0, 255]
                };
                assert_eq!(&out.rgba[(y * N + x) * 4..][..4], &want, "({x}, {y})");
                assert_eq!(out.depth[y * N + x], if inside { 3.0 } else { 5.0 });
            }
        }
        assert_eq!((out.guest_in_front, out.guest_hidden), (64, 0));
    }

    #[test]
    fn occlusion_works_from_a_host_depth_buffer_in_any_convention() {
        // The host's wall is 5 m away in its own depth buffer, which uses whichever range the
        // engine does; the guest quads sit at 3 m and 8 m.
        for range in RANGES {
            for far in [200.0, f32::INFINITY] {
                let host_cam = Camera::look_at([0.0; 3], [0.0, 0.0, -1.0], 1.0, 0.1, far, [16, 16])
                    .with_depth_range(range);
                let buffer_value = if range == DepthRange::NegOneToOne {
                    // A GL depth buffer holds (z_ndc + 1) / 2; the contract's input is z_ndc.
                    2.0 * (delinearize_depth(5.0, 0.1, far, range) * 0.5 + 0.5) - 1.0
                } else {
                    delinearize_depth(5.0, 0.1, far, range)
                };
                let host_depth = linearize_buffer(&vec![buffer_value; N * N], &host_cam);
                assert!(
                    rel(host_depth[0], 5.0) < 1e-3,
                    "{range:?}: {}",
                    host_depth[0]
                );
                let host = [255u8, 0, 0, 255].repeat(N * N);
                let (g8, d8) = quad(8.0);
                assert_eq!(composite(&host, &host_depth, &g8, &d8).unwrap().rgba, host);
                let (g3, d3) = quad(3.0);
                let covered = composite(&host, &host_depth, &g3, &d3).unwrap();
                assert_eq!(&covered.rgba[(8 * N + 8) * 4..][..4], &[0, 0, 255, 255]);
                assert_eq!(&covered.rgba[..4], &[255, 0, 0, 255]);
            }
        }
    }

    #[test]
    fn nothing_in_the_host_depth_lets_the_guest_through_and_blending_is_straight_alpha() {
        let host = [10u8, 20, 30, 255].repeat(4);
        let host_depth = [f32::INFINITY, f32::NAN, 5.0, f32::INFINITY];
        let guest = [
            200, 100, 0, 255, // opaque, nothing behind it
            200, 100, 0, 128, // half transparent over a NaN depth
            200, 100, 0, 255, // behind the host
            200, 100, 0, 0, // empty
        ]
        .to_vec();
        let guest_depth = [9.0, 9.0, 9.0, 1.0];
        let out = composite(&host, &host_depth, &guest, &guest_depth).unwrap();
        assert_eq!(&out.rgba[..4], &[200, 100, 0, 255]);
        assert_eq!(&out.rgba[4..8], &[105, 60, 15, 255]); // round((200 * 128 + 10 * 127) / 255), and so on
        assert_eq!(&out.rgba[8..12], &[10, 20, 30, 255]);
        assert_eq!(&out.rgba[12..16], &[10, 20, 30, 255]);
        assert_eq!((out.guest_in_front, out.guest_hidden), (2, 1));
        // Equal depths go to the host.
        let tie = composite(&host[..4], &[4.0], &guest[..4], &[4.0]).unwrap();
        assert_eq!(tie.rgba, host[..4]);
    }

    #[test]
    fn reprojected_guest_pixels_are_occluded_by_the_host_at_the_new_view() {
        // The guest drew a wall 8 m away from camera a. The host shows it from b, 1 m to the
        // right, where its own 5 m pillar fills the left half of the image.
        let a = cam([0.0; 3], [0.0, 0.0, -1.0]);
        let b = cam([1.0, 0.0, 0.0], [1.0, 0.0, -1.0]);
        let (g_rgba, g_depth) = grid(100, 100, |_, _| 8.0);
        let (rgba, depth) = reproject(&g_rgba, &g_depth, 100, 100, &a, &b, 100, 100).unwrap();
        let mut host = [1u8, 2, 3, 255].repeat(10_000);
        let mut host_depth = vec![f32::INFINITY; 10_000];
        for y in 0..100 {
            for x in 0..50 {
                host_depth[y * 100 + x] = 5.0;
                host[(y * 100 + x) * 4..][..3].copy_from_slice(&[255, 255, 255]);
            }
        }
        let out = composite(&host, &host_depth, &rgba, &depth).unwrap();
        // Left half: pillar. Right half: guest wall, except the strip it never rendered.
        assert_eq!(&out.rgba[(50 * 100 + 10) * 4..][..4], &[255, 255, 255, 255]);
        assert_eq!(
            out.rgba[(50 * 100 + 60) * 4 + 2],
            7,
            "guest pixel (B channel is 7)"
        );
        assert_eq!(
            &out.rgba[(50 * 100 + 99) * 4..][..4],
            &[1, 2, 3, 255],
            "hole shows the host"
        );
    }

    #[test]
    fn the_camera_ring_forgets_the_oldest_and_never_finds_seq_0() {
        let mut ring = CameraRing::new(3);
        let c = |x: f32| cam([x, 0.0, 0.0], [x, 0.0, -1.0]);
        for seq in 1..=5u32 {
            ring.push(seq, c(seq as f32));
        }
        assert!(ring.get(1).is_none() && ring.get(2).is_none());
        assert_eq!(ring.get(4).unwrap().eye()[0].round(), 4.0);
        assert_eq!(ring.latest().unwrap().eye()[0].round(), 5.0);
        assert!(ring.get(0).is_none() && ring.get(99).is_none());
        ring.push(0, c(0.0));
        assert!(ring.get(0).is_none(), "0 means 'no camera'");
        assert_eq!(CameraRing::default().cap, 32);
    }
}
