// Reprojection: three compute passes (clear, splat_depth, splat_index). See the header of common.wgsl.

@group(0) @binding(1) var gcolor: texture_2d<u32>;
@group(0) @binding(2) var gdepth: texture_2d<f32>;

fn g_depth(p: vec2<i32>) -> f32 {
    if (params.origin_flags.z & F_DEPTH) == 0u {
        // No depth plane: a stand-in so the frame can still be resampled; never depth tested.
        return 1.0;
    }
    return textureLoad(gdepth, p, 0).r;
}

// ---------------------------------------------------------------------------------------------
// Reprojection

struct Warped {
    ok: bool,
    x: f32,
    y: f32,
    t: f32,
}

// A guest pixel-grid point (px, py) at linear depth t, in destination window coordinates.
fn warp(px: f32, py: f32, t: f32) -> Warped {
    var w = Warped(false, 0.0, 0.0, 0.0);
    let fw = f32(params.sizes.x);
    let fh = f32(params.sizes.y);
    let nx = px / fw * 2.0 - 1.0;
    let ny = 1.0 - py / fh * 2.0;
    // The view-space point (x, y, -t) that projects to (nx, ny): solve the first two rows of the
    // render projection (n * w_clip = clip_n, both affine in x and y once z is fixed).
    let z = -t;
    let p0 = params.proj[0];
    let p1 = params.proj[1];
    let p3 = params.proj[3];
    let a1 = p0.x - nx * p3.x;
    let b1 = p0.y - nx * p3.y;
    let r1 = nx * (p3.z * z + p3.w) - (p0.z * z + p0.w);
    let a2 = p1.x - ny * p3.x;
    let b2 = p1.y - ny * p3.y;
    let r2 = ny * (p3.z * z + p3.w) - (p1.z * z + p1.w);
    let det = a1 * b2 - a2 * b1;
    if !finite(det) || abs(det) <= 1e-12 {
        return w;
    }
    let pv = vec4<f32>((r1 * b2 - r2 * b1) / det, (a1 * r2 - a2 * r1) / det, z, 1.0);
    // Into the present camera.
    let t2 = -dot(params.rel[2], pv);
    let c = vec4<f32>(
        dot(params.clip[0], pv),
        dot(params.clip[1], pv),
        dot(params.clip[2], pv),
        dot(params.clip[3], pv),
    );
    if !(t2 > 1e-9 && c.w > 1e-12) {
        return w;
    }
    let dw = f32(params.sizes.z);
    let dh = f32(params.sizes.w);
    let wx = (c.x / c.w * 0.5 + 0.5) * dw;
    let wy = (0.5 - c.y / c.w * 0.5) * dh;
    if !finite(wx) || !finite(wy) || !finite(t2) {
        return w;
    }
    w.ok = true;
    w.x = wx;
    w.y = wy;
    w.t = t2;
    return w;
}

// The depth a pixel-grid corner is warped at: the average over the pixels around it whose depth is
// within 10% of t.
fn corner_depth(cx: i32, cy: i32, t: f32) -> f32 {
    let w = i32(params.sizes.x);
    let h = i32(params.sizes.y);
    var sum = 0.0;
    var n = 0.0;
    for (var y = max(cy - 1, 0); y <= min(cy, h - 1); y++) {
        for (var x = max(cx - 1, 0); x <= min(cx, w - 1); x++) {
            let p = vec2<i32>(x, y);
            let d = g_depth(p);
            if textureLoad(gcolor, p, 0).a != 0u && finite(d) && abs(d - t) <= 0.1 * t {
                sum += d;
                n += 1.0;
            }
        }
    }
    return sum / n;
}

@group(0) @binding(3) var<storage, read_write> depth_bits: array<atomic<u32>>;
@group(0) @binding(4) var<storage, read_write> index_buf: array<atomic<u32>>;
// per guest pixel: x0 | x1 << 16, y0 | y1 << 16, depth bits, 1 if it has a footprint
@group(0) @binding(5) var<storage, read_write> footprints: array<vec4<u32>>;

@compute @workgroup_size(8, 8)
fn clear(@builtin(global_invocation_id) id: vec3<u32>) {
    if id.x >= params.sizes.z || id.y >= params.sizes.w {
        return;
    }
    let j = id.y * params.sizes.z + id.x;
    atomicStore(&depth_bits[j], EMPTY);
    atomicStore(&index_buf[j], EMPTY);
}

@compute @workgroup_size(8, 8)
fn splat_depth(@builtin(global_invocation_id) id: vec3<u32>) {
    if id.x >= params.sizes.x || id.y >= params.sizes.y {
        return;
    }
    let i = id.y * params.sizes.x + id.x;
    footprints[i] = vec4<u32>(0u, 0u, 0u, 0u);
    let p = vec2<i32>(id.xy);
    let t = g_depth(p);
    if textureLoad(gcolor, p, 0).a == 0u || !finite(t) || t <= 0.0 {
        return;
    }
    let px = f32(id.x);
    let py = f32(id.y);
    let c = warp(px + 0.5, py + 0.5, t);
    if !c.ok {
        return;
    }
    // The pixel's square, warped: every destination pixel centre inside its bounding box. Falls
    // back to the pixel holding the centre if the box is degenerate.
    var x0 = floor(c.x);
    var x1 = x0;
    var y0 = floor(c.y);
    var y1 = y0;
    let a = warp(px, py, corner_depth(p.x, p.y, t));
    let b = warp(px + 1.0, py, corner_depth(p.x + 1, p.y, t));
    let cc = warp(px, py + 1.0, corner_depth(p.x, p.y + 1, t));
    let d = warp(px + 1.0, py + 1.0, corner_depth(p.x + 1, p.y + 1, t));
    if a.ok && b.ok && cc.ok && d.ok {
        let minx = min(min(a.x, b.x), min(cc.x, d.x));
        let maxx = max(max(a.x, b.x), max(cc.x, d.x));
        let miny = min(min(a.y, b.y), min(cc.y, d.y));
        let maxy = max(max(a.y, b.y), max(cc.y, d.y));
        let fx0 = ceil(minx - 0.5);
        let fx1 = floor(maxx - 0.5);
        let fy0 = ceil(miny - 0.5);
        let fy1 = floor(maxy - 0.5);
        if fx0 <= fx1 && fy0 <= fy1 && maxx - minx <= MAX_FOOTPRINT && maxy - miny <= MAX_FOOTPRINT {
            x0 = fx0;
            x1 = fx1;
            y0 = fy0;
            y1 = fy1;
        }
    }
    x0 = max(x0, 0.0);
    x1 = min(x1, f32(params.sizes.z) - 1.0);
    y0 = max(y0, 0.0);
    y1 = min(y1, f32(params.sizes.w) - 1.0);
    if x0 > x1 || y0 > y1 || !finite(c.t) {
        return;
    }
    let bits = bitcast<u32>(c.t);
    let ix0 = u32(x0);
    let ix1 = u32(x1);
    let iy0 = u32(y0);
    let iy1 = u32(y1);
    footprints[i] = vec4<u32>(ix0 | (ix1 << 16u), iy0 | (iy1 << 16u), bits, 1u);
    for (var dy = iy0; dy <= iy1; dy++) {
        for (var dx = ix0; dx <= ix1; dx++) {
            atomicMin(&depth_bits[dy * params.sizes.z + dx], bits);
        }
    }
}

@compute @workgroup_size(8, 8)
fn splat_index(@builtin(global_invocation_id) id: vec3<u32>) {
    if id.x >= params.sizes.x || id.y >= params.sizes.y {
        return;
    }
    let i = id.y * params.sizes.x + id.x;
    let f = footprints[i];
    if f.w == 0u {
        return;
    }
    let ix0 = f.x & 0xffffu;
    let ix1 = f.x >> 16u;
    let iy0 = f.y & 0xffffu;
    let iy1 = f.y >> 16u;
    for (var dy = iy0; dy <= iy1; dy++) {
        for (var dx = ix0; dx <= ix1; dx++) {
            let j = dy * params.sizes.z + dx;
            if atomicLoad(&depth_bits[j]) == f.z {
                atomicMin(&index_buf[j], i);
            }
        }
    }
}

