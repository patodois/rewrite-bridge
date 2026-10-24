// GPU version of rewrite-bridge's `link::compose` reference (contract section 9.7): reprojects a
// guest frame from the camera it was rendered with to the camera the host presents now, then
// composites it over the host's image with a per-pixel depth test.
//
// The same rules as `compose::reproject` and `compose::composite`, line by line:
//
//   splat_depth  a thread per guest pixel. Unprojects the pixel with the render camera at its linear
//                depth, moves it to the present camera and finds the destination pixels its warped
//                square covers (corners warped at the average depth of the neighbours within 10%;
//                one pixel if the box is degenerate or wider than 16 pixels). Keeps the nearest
//                depth per destination pixel (atomicMin on the f32 bits, which order like the
//                floats for positive finite values) and records the footprint for the next pass.
//   splat_index  a thread per guest pixel again. Of the guest pixels whose depth is the winning
//                depth of a destination pixel, the lowest index wins, which is what the CPU's
//                row-major loop with a strict `<` keeps.
//   fragment     per host pixel: the guest pixel that won (or the direct pixel when no warp is
//                needed), tested against the host's own depth buffer, blended with the straight
//                alpha "over" in encoded (sRGB) values using the same integer arithmetic.
//
// WGSL has no infinity literal and Metal may compile with fast math, so non-finite values are
// recognised by their bits and "nothing here" is kept as a flag or as the sentinel EMPTY.

// This part is shared by warp.wgsl and composite.wgsl; Rust concatenates it in front of each.

struct Params {
    // guest width, guest height, destination (host viewport) width, height
    sizes: vec4<u32>,
    // viewport origin x, y in the host target; flags; unused
    origin_flags: vec4<u32>,
    // host depth buffer: linear depth t = b / (d + a); a, b, unused, unused
    host: vec4<f32>,
    // rows of the render camera's projection
    proj: array<vec4<f32>, 4>,
    // rows of render view space -> present view space
    rel: array<vec4<f32>, 4>,
    // rows of render view space -> present clip space
    clip: array<vec4<f32>, 4>,
}

// flags
const F_WARP: u32 = 1u;     // go through the reprojection buffers
const F_DEPTH: u32 = 2u;    // the guest frame has a depth plane (depth test on)
const F_SRGB: u32 = 4u;     // the host target stores linear values of an sRGB-encoded image

const EMPTY: u32 = 0xffffffffu;
const MAX_FOOTPRINT: f32 = 16.0;

@group(0) @binding(0) var<uniform> params: Params;

fn finite(t: f32) -> bool {
    return (bitcast<u32>(t) & 0x7f800000u) != 0x7f800000u;
}
