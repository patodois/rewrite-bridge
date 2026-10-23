#![allow(dead_code)]
//! A tiny CPU triangle rasteriser shared by `fake_guest` and `fake_host`, just enough to draw a lit
//! cube, a pillar and a checkered floor with a z-buffer. It keeps depth the way a pixel file
//! carries it: linear metres in front of the camera, `+inf` where nothing was drawn.

use rewrite_bridge::link::Camera;

/// One triangle with a flat normal (counter-clockwise from its front).
#[derive(Clone, Copy)]
pub struct Tri {
    pub v: [[f32; 3]; 3],
    pub n: [f32; 3],
}

/// Colour and linear depth of an image being drawn.
pub struct Target {
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
    pub depth: Vec<f32>,
}

impl Target {
    pub fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            rgba: vec![0; w * h * 4],
            depth: vec![f32::INFINITY; w * h],
        }
    }

    pub fn clear(&mut self, rgba: [u8; 4]) {
        for px in self.rgba.as_chunks_mut::<4>().0.iter_mut() {
            px.copy_from_slice(&rgba);
        }
        self.depth.fill(f32::INFINITY);
    }

    /// A vertical gradient with no depth, like a sky.
    pub fn sky(&mut self, top: [u8; 3], bottom: [u8; 3]) {
        for y in 0..self.h {
            let k = y as f32 / (self.h - 1).max(1) as f32;
            let c: [u8; 3] = std::array::from_fn(|i| {
                (f32::from(top[i]) * (1.0 - k) + f32::from(bottom[i]) * k) as u8
            });
            for x in 0..self.w {
                self.rgba[(y * self.w + x) * 4..][..4].copy_from_slice(&[c[0], c[1], c[2], 255]);
            }
        }
        self.depth.fill(f32::INFINITY);
    }

    /// Draws triangles seen from `cam`; `shade(world_position, normal_facing_the_camera)` gives
    /// each pixel's colour.
    pub fn draw(
        &mut self,
        cam: &Camera,
        tris: &[Tri],
        shade: &dyn Fn([f32; 3], [f32; 3]) -> [u8; 3],
    ) {
        let eye = cam.eye();
        for tri in tris {
            let mut poly: Vec<Vert> = tri.v.iter().map(|&p| Vert::new(cam, p)).collect();
            // Clip against the near plane (w >= near), the only plane that matters for a
            // rasteriser that clamps to the viewport.
            poly = clip_near(&poly, cam.near.max(1e-3));
            if poly.len() < 3 {
                continue;
            }
            let mut n = tri.n;
            let to_eye = sub(eye, tri.v[0]);
            if dot(n, to_eye) < 0.0 {
                n = [-n[0], -n[1], -n[2]];
            }
            for i in 1..poly.len() - 1 {
                self.fill(&[poly[0], poly[i], poly[i + 1]], n, shade);
            }
        }
    }

    fn fill(&mut self, v: &[Vert; 3], n: [f32; 3], shade: &dyn Fn([f32; 3], [f32; 3]) -> [u8; 3]) {
        let (w, h) = (self.w as f32, self.h as f32);
        let s: [[f32; 2]; 3] = std::array::from_fn(|i| {
            [
                (v[i].clip[0] / v[i].clip[3] * 0.5 + 0.5) * w,
                (0.5 - v[i].clip[1] / v[i].clip[3] * 0.5) * h,
            ]
        });
        let area = edge(s[0], s[1], s[2]);
        if area.abs() < 1e-9 {
            return;
        }
        let min_x = s
            .iter()
            .map(|p| p[0])
            .fold(f32::INFINITY, f32::min)
            .floor()
            .max(0.0) as usize;
        let max_x = s
            .iter()
            .map(|p| p[0])
            .fold(f32::NEG_INFINITY, f32::max)
            .ceil()
            .min(w) as usize;
        let min_y = s
            .iter()
            .map(|p| p[1])
            .fold(f32::INFINITY, f32::min)
            .floor()
            .max(0.0) as usize;
        let max_y = s
            .iter()
            .map(|p| p[1])
            .fold(f32::NEG_INFINITY, f32::max)
            .ceil()
            .min(h) as usize;
        let inv_w: [f32; 3] = std::array::from_fn(|i| 1.0 / v[i].clip[3]);
        for y in min_y..max_y {
            for x in min_x..max_x {
                let p = [x as f32 + 0.5, y as f32 + 0.5];
                let b0 = edge(s[1], s[2], p) / area;
                let b1 = edge(s[2], s[0], p) / area;
                let b2 = edge(s[0], s[1], p) / area;
                if b0 < 0.0 || b1 < 0.0 || b2 < 0.0 {
                    continue;
                }
                // Perspective-correct: 1/w and attribute/w are linear on screen.
                let iw = b0 * inv_w[0] + b1 * inv_w[1] + b2 * inv_w[2];
                let t = 1.0 / iw;
                let i = y * self.w + x;
                if t >= self.depth[i] {
                    continue;
                }
                let world: [f32; 3] = std::array::from_fn(|k| {
                    (b0 * v[0].world[k] * inv_w[0]
                        + b1 * v[1].world[k] * inv_w[1]
                        + b2 * v[2].world[k] * inv_w[2])
                        * t
                });
                let c = shade(world, n);
                self.depth[i] = t;
                self.rgba[i * 4..i * 4 + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Vert {
    clip: [f32; 4],
    world: [f32; 3],
}

impl Vert {
    fn new(cam: &Camera, p: [f32; 3]) -> Self {
        let v = mul_vec(&cam.view, [p[0], p[1], p[2], 1.0]);
        Self {
            clip: mul_vec(&cam.proj, v),
            world: p,
        }
    }
}

fn clip_near(poly: &[Vert], near: f32) -> Vec<Vert> {
    let mut out = Vec::new();
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
        let (ina, inb) = (a.clip[3] >= near, b.clip[3] >= near);
        if ina {
            out.push(a);
        }
        if ina != inb {
            let k = (near - a.clip[3]) / (b.clip[3] - a.clip[3]);
            out.push(Vert {
                clip: std::array::from_fn(|c| a.clip[c] + (b.clip[c] - a.clip[c]) * k),
                world: std::array::from_fn(|c| a.world[c] + (b.world[c] - a.world[c]) * k),
            });
        }
    }
    out
}

fn mul_vec(m: &[f32; 16], v: [f32; 4]) -> [f32; 4] {
    std::array::from_fn(|r| (0..4).map(|c| m[c * 4 + r] * v[c]).sum())
}

fn edge(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> f32 {
    (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Twelve triangles of a box centred at `center` with half-extents `half`, turned `yaw` radians
/// about +Y.
pub fn cuboid(center: [f32; 3], half: [f32; 3], yaw: f32) -> Vec<Tri> {
    let (s, c) = yaw.sin_cos();
    let rot = |p: [f32; 3]| [c * p[0] + s * p[2], p[1], -s * p[0] + c * p[2]];
    let corner = |sx: f32, sy: f32, sz: f32| {
        let r = rot([sx * half[0], sy * half[1], sz * half[2]]);
        [center[0] + r[0], center[1] + r[1], center[2] + r[2]]
    };
    // Each face: outward normal and its four corners, counter-clockwise from outside.
    let faces: [([f32; 3], [[f32; 3]; 4]); 6] = [
        (
            [1.0, 0.0, 0.0],
            [
                corner(1., -1., 1.),
                corner(1., -1., -1.),
                corner(1., 1., -1.),
                corner(1., 1., 1.),
            ],
        ),
        (
            [-1.0, 0.0, 0.0],
            [
                corner(-1., -1., -1.),
                corner(-1., -1., 1.),
                corner(-1., 1., 1.),
                corner(-1., 1., -1.),
            ],
        ),
        (
            [0.0, 1.0, 0.0],
            [
                corner(-1., 1., 1.),
                corner(1., 1., 1.),
                corner(1., 1., -1.),
                corner(-1., 1., -1.),
            ],
        ),
        (
            [0.0, -1.0, 0.0],
            [
                corner(-1., -1., -1.),
                corner(1., -1., -1.),
                corner(1., -1., 1.),
                corner(-1., -1., 1.),
            ],
        ),
        (
            [0.0, 0.0, 1.0],
            [
                corner(-1., -1., 1.),
                corner(1., -1., 1.),
                corner(1., 1., 1.),
                corner(-1., 1., 1.),
            ],
        ),
        (
            [0.0, 0.0, -1.0],
            [
                corner(1., -1., -1.),
                corner(-1., -1., -1.),
                corner(-1., 1., -1.),
                corner(1., 1., -1.),
            ],
        ),
    ];
    let mut tris = Vec::new();
    for (n, q) in faces {
        let n = rot(n);
        tris.push(Tri {
            v: [q[0], q[1], q[2]],
            n,
        });
        tris.push(Tri {
            v: [q[0], q[2], q[3]],
            n,
        });
    }
    tris
}

/// A horizontal quad at height `y` over `x` and `z` ranges, facing up.
pub fn floor(y: f32, x: [f32; 2], z: [f32; 2]) -> Vec<Tri> {
    let q = [
        [x[0], y, z[1]],
        [x[1], y, z[1]],
        [x[1], y, z[0]],
        [x[0], y, z[0]],
    ];
    let n = [0.0, 1.0, 0.0];
    vec![
        Tri {
            v: [q[0], q[1], q[2]],
            n,
        },
        Tri {
            v: [q[0], q[2], q[3]],
            n,
        },
    ]
}

/// Direction the light shines from (towards the light), unit length.
const LIGHT: [f32; 3] = [0.40, 0.80, 0.45];

/// Flat Lambert shading of `base` with some ambient light.
pub fn lit(base: [f32; 3], normal: [f32; 3]) -> [u8; 3] {
    let l = (dot(LIGHT, LIGHT)).sqrt();
    let k = (dot(normal, LIGHT) / l).max(0.0);
    std::array::from_fn(|i| (base[i] * (0.30 + 0.70 * k)).clamp(0.0, 255.0) as u8)
}

/// Writes RGBA as a binary PPM (P6), dropping alpha.
pub fn write_ppm(path: &std::path::Path, w: usize, h: usize, rgba: &[u8]) -> std::io::Result<()> {
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in rgba.as_chunks::<4>().0.iter() {
        out.extend_from_slice(&px[..3]);
    }
    std::fs::write(path, out)
}
