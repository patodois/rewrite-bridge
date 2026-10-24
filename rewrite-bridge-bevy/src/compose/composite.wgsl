// Compositing: one fullscreen pass over the host target.

// ---------------------------------------------------------------------------------------------
// Compositing

@group(0) @binding(1) var host_color: texture_2d<f32>;
@group(0) @binding(2) var host_depth: texture_depth_2d;
@group(0) @binding(3) var guest_color: texture_2d<u32>;
@group(0) @binding(4) var guest_depth: texture_2d<f32>;
@group(0) @binding(5) var<storage, read> warped_depth: array<u32>;
@group(0) @binding(6) var<storage, read> warped_index: array<u32>;

fn g_depth(p: vec2<i32>) -> f32 {
    if (params.origin_flags.z & F_DEPTH) == 0u {
        // No depth plane: a stand-in, never depth tested.
        return 1.0;
    }
    return textureLoad(guest_depth, p, 0).r;
}

@vertex
fn vertex(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    return vec4<f32>(x * 2.0 - 1.0, y * 2.0 - 1.0, 0.0, 1.0);
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let c0 = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    let lo = c0 * 12.92;
    let hi = 1.055 * pow(c0, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(hi, lo, c0 <= vec3<f32>(0.0031308));
}

@fragment
fn fragment(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let target_px = vec2<i32>(pos.xy);
    let host = textureLoad(host_color, target_px, 0);
    let flags = params.origin_flags.z;
    let local = target_px - vec2<i32>(params.origin_flags.xy);
    if local.x < 0 || local.y < 0 || local.x >= i32(params.sizes.z) || local.y >= i32(params.sizes.w) {
        return host;
    }
    let j = u32(local.y) * params.sizes.z + u32(local.x);

    // The guest pixel for this host pixel, and its linear depth.
    var g: vec4<u32>;
    var gd: f32;
    if (flags & F_WARP) != 0u {
        let idx = warped_index[j];
        if idx == EMPTY {
            return host;
        }
        let sw = params.sizes.x;
        let gp = vec2<i32>(i32(idx % sw), i32(idx / sw));
        g = textureLoad(guest_color, gp, 0);
        gd = bitcast<f32>(warped_depth[j]);
    } else {
        let gp = local;
        g = textureLoad(guest_color, gp, 0);
        gd = g_depth(gp);
        if g.a == 0u || !finite(gd) || gd <= 0.0 {
            return host;
        }
    }
    if g.a == 0u {
        return host;
    }

    if (flags & F_DEPTH) != 0u {
        // The host's depth in linear metres. A depth that gives no positive finite distance (a
        // cleared buffer) means the host drew nothing there.
        let d = textureLoad(host_depth, target_px, 0);
        let den = d + params.host.x;
        if den != 0.0 {
            let th = params.host.y / den;
            if finite(th) && th > 0.0 && gd >= th {
                return host;
            }
        }
    }

    // Straight-alpha "over" on the encoded 8-bit values, with the reference's integer arithmetic.
    var hrgb = host.rgb;
    if (flags & F_SRGB) != 0u {
        hrgb = linear_to_srgb(hrgb);
    }
    let hb = vec3<u32>(round(clamp(hrgb, vec3<f32>(0.0), vec3<f32>(1.0)) * 255.0));
    let ha = u32(round(clamp(host.a, 0.0, 1.0) * 255.0));
    let a = g.a;
    let out_rgb = (g.rgb * a + hb * (255u - a) + vec3<u32>(127u)) / 255u;
    let out_a = min(a + (ha * (255u - a) + 127u) / 255u, 255u);
    var rgb = vec3<f32>(out_rgb) / 255.0;
    if (flags & F_SRGB) != 0u {
        rgb = srgb_to_linear(rgb);
    }
    return vec4<f32>(rgb, f32(out_a) / 255.0);
}
