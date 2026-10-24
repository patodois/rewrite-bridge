//! The GPU compositing against the CPU reference, `rewrite_bridge::link::compose`, on scenes both
//! can run: the same host image and depth buffer, the same guest frame and cameras go through
//! `reproject` + `composite` on the CPU and through the shaders on the GPU, and the two images are
//! compared pixel by pixel.
//!
//! Without a usable GPU adapter (a CI machine, say) the tests print `SKIP` and pass.
//!
//! The tolerances, and why:
//!
//! - **Compositing alone** (the guest already at the host's view, no warp) must match **exactly**:
//!   the shader uses the reference's integer arithmetic, and both sides test the same linear depths.
//!   The host depth buffer is converted with `t = b / (d + a)` in `f32` on the GPU and with the
//!   reference's `f64` formulas on the CPU, which can differ in the last bits of `t`; the test scenes
//!   keep guest and host depths at least a few centimetres apart except where a pair is *meant* to be
//!   close, and the comparison tolerates none of those flipping.
//! - **Through an sRGB target** the host pixel goes bytes -> linear (hardware) -> bytes (shader) and
//!   the result bytes -> linear -> bytes (shader, hardware), which may land one step off: at most 1.
//! - **With reprojection** the warp runs in `f32` (the matrices are built in `f64` on the CPU and
//!   rounded) where the reference is `f64` throughout, so a footprint edge that falls within a
//!   rounding error of a pixel centre can fall on the other side. That moves single pixels along
//!   edges, never regions: the test bounds the share of pixels that differ at all and requires every
//!   pixel that differs to sit next to one that the CPU also has an edge at (the footprint of the
//!   neighbours differ), see [`compare_warp`].

use std::sync::OnceLock;

use bevy::render::render_resource::{
    BindGroupEntry, BindingResource, BufferDescriptor, BufferUsages, Extent3d, MapMode, Operations,
    Origin3d, PipelineLayoutDescriptor, PollType, RenderPassDepthStencilAttachment,
    RenderPassDescriptor, ShaderModuleDescriptor, ShaderSource, ShaderStages, StoreOp,
    TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, TextureAspect,
    TextureDescriptor, TextureDimension, TextureFormat, TextureUsages, TextureViewDescriptor,
};
use bevy::render::renderer::{initialize_renderer, RenderDevice, RenderQueue};
use bevy::render::settings::{Backends, WgpuSettings};
use rewrite_bridge::link::compose::{composite, delinearize_depth, linearize_buffer, reproject};
use rewrite_bridge::link::{Camera, DepthRange};

use super::gpu::{ComposeParams, Engine, GuestGpu, WarpBuffers, WarpMatrices};
use super::{host_depth_terms, GuestFrame};

// ---------------------------------------------------------------------------------------------
// A headless GPU

pub(super) fn gpu() -> Option<&'static (RenderDevice, RenderQueue)> {
    static GPU: OnceLock<Option<(RenderDevice, RenderQueue)>> = OnceLock::new();
    GPU.get_or_init(|| {
        std::panic::catch_unwind(|| {
            let settings = WgpuSettings {
                backends: Some(Backends::all()),
                ..Default::default()
            };
            let resources =
                bevy::tasks::block_on(initialize_renderer(Backends::all(), None, &settings));
            (resources.0, resources.1)
        })
        .ok()
    })
    .as_ref()
}

macro_rules! gpu_or_skip {
    () => {
        match gpu() {
            Some(g) => g,
            None => {
                eprintln!("SKIP: no GPU adapter");
                return;
            }
        }
    };
}

const FILL_DEPTH: &str = "
@group(0) @binding(0) var src: texture_2d<f32>;
@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    return vec4<f32>(f32((i << 1u) & 2u) * 2.0 - 1.0, f32(i & 2u) * 2.0 - 1.0, 0.0, 1.0);
}
struct Out { @builtin(frag_depth) d: f32 }
@fragment fn fs(@builtin(position) p: vec4<f32>) -> Out {
    return Out(textureLoad(src, vec2<i32>(p.xy), 0).r);
}";

fn texture(
    device: &RenderDevice,
    (w, h): (u32, u32),
    format: TextureFormat,
    usage: TextureUsages,
) -> bevy::render::render_resource::Texture {
    device.create_texture(&TextureDescriptor {
        label: Some("test"),
        size: Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

fn write(
    queue: &RenderQueue,
    texture: &bevy::render::render_resource::Texture,
    (w, h): (u32, u32),
    data: &[u8],
) {
    queue.write_texture(
        TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: Origin3d::ZERO,
            aspect: TextureAspect::All,
        },
        data,
        TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 4),
            rows_per_image: Some(h),
        },
        Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
}

/// Everything a run needs on the host's side: an RGBA8 image (bytes as stored, i.e. sRGB-encoded
/// values when `format` is an sRGB one) and a reversed-Z depth buffer.
struct Host<'a> {
    rgba: &'a [u8],
    /// Depth buffer values, as `delinearize_depth` gives for `Reversed`.
    d: &'a [f32],
    size: (u32, u32),
    format: TextureFormat,
}

struct Run<'a> {
    host: Host<'a>,
    guest: &'a GuestFrame,
    params: ComposeParams,
}

/// Fills `depth_view` (a `Depth32Float` texture) with the values of `d_src` (an `R32Float` texture): a
/// depth texture can only be written by drawing, so a pass copies them over with `frag_depth`.
fn fill_depth(
    device: &RenderDevice,
    encoder: &mut bevy::render::render_resource::CommandEncoder,
    d_src: &bevy::render::render_resource::Texture,
    depth_view: &bevy::render::render_resource::TextureView,
) {
    let module = device.create_and_validate_shader_module(ShaderModuleDescriptor {
        label: Some("fill_depth"),
        source: ShaderSource::Wgsl(FILL_DEPTH.into()),
    });
    let layout = device.create_bind_group_layout(
        "fill_depth_layout",
        &[bevy::render::render_resource::BindGroupLayoutEntry {
            binding: 0,
            visibility: ShaderStages::FRAGMENT,
            ty: bevy::render::render_resource::BindingType::Texture {
                sample_type: bevy::render::render_resource::TextureSampleType::Float {
                    filterable: false,
                },
                view_dimension: bevy::render::render_resource::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        }],
    );
    let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(
        &bevy::render::render_resource::RawRenderPipelineDescriptor {
            label: Some("fill_depth"),
            layout: Some(&pipeline_layout),
            vertex: bevy::render::render_resource::RawVertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(bevy::render::render_resource::RawFragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[],
            }),
            primitive: Default::default(),
            depth_stencil: Some(bevy::render::render_resource::DepthStencilState {
                format: TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(bevy::render::render_resource::CompareFunction::Always),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        },
    );
    let d_view = d_src.create_view(&TextureViewDescriptor::default());
    let bind = device.create_bind_group(
        "fill_depth_bind",
        &layout,
        &[BindGroupEntry {
            binding: 0,
            resource: BindingResource::TextureView(&d_view),
        }],
    );
    let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
        label: Some("fill_depth"),
        color_attachments: &[],
        depth_stencil_attachment: Some(RenderPassDepthStencilAttachment {
            view: depth_view,
            depth_ops: Some(Operations {
                load: bevy::render::render_resource::LoadOp::Clear(0.0),
                store: StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_pipeline(&pipeline);
    pass.set_bind_group(0, &bind, &[]);
    pass.draw(0..3, 0..1);
}

/// Runs the GPU passes and reads the target back (`size.0 * size.1 * 4` bytes, tightly packed).
fn run_gpu(device: &RenderDevice, queue: &RenderQueue, run: &Run) -> Vec<u8> {
    let (w, h) = run.host.size;
    let mut engine = Engine::new(device);
    engine.prepare_format(device, run.host.format);
    let mut guest = GuestGpu::new(
        device,
        (run.guest.width, run.guest.height),
        run.guest.depth.is_some(),
    );
    guest.upload(queue, run.guest);

    let host_tex = texture(
        device,
        (w, h),
        run.host.format,
        TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
    );
    write(queue, &host_tex, (w, h), run.host.rgba);
    let host_view = host_tex.create_view(&TextureViewDescriptor::default());

    // The host depth buffer: a Depth32Float texture can only be written by drawing, so a pass copies
    // the values over with frag_depth.
    let d_src = texture(
        device,
        (w, h),
        TextureFormat::R32Float,
        TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
    );
    write(queue, &d_src, (w, h), bytemuck::cast_slice(run.host.d));
    let depth_tex = texture(
        device,
        (w, h),
        TextureFormat::Depth32Float,
        TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT,
    );
    let depth_view = depth_tex.create_view(&TextureViewDescriptor::default());
    let target = texture(
        device,
        (w, h),
        run.host.format,
        TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
    );
    let target_view = target.create_view(&TextureViewDescriptor::default());

    let mut encoder = Engine::new_encoder(device);
    fill_depth(device, &mut encoder, &d_src, &depth_view);

    let buffers = run
        .params
        .warp
        .is_some()
        .then(|| WarpBuffers::new(device, run.params.guest_size, run.params.dst_size));
    if let Some(b) = &buffers {
        engine.encode_warp(device, &mut encoder, &run.params, &guest, b);
    }
    assert!(engine.encode_composite(
        device,
        &mut encoder,
        &run.params,
        &guest,
        buffers.as_ref(),
        &host_view,
        &depth_view,
        &target_view,
        run.host.format,
    ));

    // Read the target back
    let row = (w * 4).div_ceil(256) * 256;
    let out = device.create_buffer(&BufferDescriptor {
        label: Some("readback"),
        size: u64::from(row * h),
        usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: Origin3d::ZERO,
            aspect: TextureAspect::All,
        },
        TexelCopyBufferInfo {
            buffer: &out,
            layout: TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(h),
            },
        },
        Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);
    let slice = out.slice(..);
    slice.map_async(MapMode::Read, |r| r.expect("map"));
    device
        .poll(PollType::wait_indefinitely())
        .expect("the GPU finished");
    let mapped = slice.get_mapped_range().expect("mapped");
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h as usize {
        pixels.extend_from_slice(&mapped[y * row as usize..][..w as usize * 4]);
    }
    drop(mapped);
    out.unmap();
    pixels
}

// ---------------------------------------------------------------------------------------------
// Scenes

const NEAR: f32 = 0.1;

fn camera(eye: [f32; 3], target: [f32; 3], size: (u32, u32), far: f32) -> Camera {
    Camera::look_at(
        eye,
        target,
        60.0f32.to_radians(),
        NEAR,
        far,
        [size.0, size.1],
    )
    .with_depth_range(DepthRange::Reversed)
}

/// What a guest renders: a ray-cast of a few shapes in the host's world, seen through `cam`. Linear
/// depth is the distance along the view axis (what the contract carries); empty pixels have alpha 0 and
/// `+inf` depth. A glass pane (alpha 128) and a sphere with smooth shading give partial coverage and
/// colour gradients; a ground plane gives a surface that stretches with distance.
pub(super) fn render_guest(
    cam: &Camera,
    (w, h): (u32, u32),
    with_depth: bool,
) -> (Vec<u8>, Vec<f32>) {
    let eye = cam.eye();
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let mut depth = vec![f32::INFINITY; (w * h) as usize];
    // view -> world rotation: the transpose of the view matrix's 3 x 3
    let v = &cam.view;
    let world = |d: [f32; 3]| -> [f32; 3] {
        std::array::from_fn(|r| (0..3).map(|c| v[r * 4 + c] * d[c]).sum())
    };
    for y in 0..h {
        for x in 0..w {
            let nx = (x as f32 + 0.5) / w as f32 * 2.0 - 1.0;
            let ny = 1.0 - (y as f32 + 0.5) / h as f32 * 2.0;
            // a view-space direction with z = -1, so the ray parameter is the linear depth
            let dir = world([nx / cam.proj[0], ny / cam.proj[5], -1.0]);
            let mut best: Option<(f32, [u8; 4])> = None;
            let mut consider = |t: f32, c: [u8; 4]| {
                if t > NEAR && t.is_finite() && best.is_none_or(|(b, _)| t < b) {
                    best = Some((t, c));
                }
            };
            // sphere, centre (0, 1, -4), radius 1.1
            {
                let c = [0.0, 1.0, -4.0];
                let oc = [eye[0] - c[0], eye[1] - c[1], eye[2] - c[2]];
                let a: f32 = dir.iter().map(|d| d * d).sum();
                let b: f32 = 2.0 * (0..3).map(|i| oc[i] * dir[i]).sum::<f32>();
                let cc: f32 = oc.iter().map(|o| o * o).sum::<f32>() - 1.1 * 1.1;
                let disc = b * b - 4.0 * a * cc;
                if disc >= 0.0 {
                    let t = (-b - disc.sqrt()) / (2.0 * a);
                    let p: [f32; 3] = std::array::from_fn(|i| eye[i] + t * dir[i]);
                    let n: [f32; 3] = std::array::from_fn(|i| (p[i] - c[i]) / 1.1);
                    consider(
                        t,
                        [
                            (128.0 + 127.0 * n[0]) as u8,
                            (128.0 + 127.0 * n[1]) as u8,
                            (128.0 + 127.0 * n[2]) as u8,
                            255,
                        ],
                    );
                }
            }
            // a plane z = z0 limited to a rectangle
            let mut rect = |z0: f32, x: (f32, f32), y: (f32, f32), c: [u8; 4]| {
                if dir[2].abs() > 1e-6 {
                    let t = (z0 - eye[2]) / dir[2];
                    let (px, py) = (eye[0] + t * dir[0], eye[1] + t * dir[1]);
                    if px >= x.0 && px <= x.1 && py >= y.0 && py <= y.1 {
                        consider(t, c);
                    }
                }
            };
            rect(-3.0, (1.0, 2.5), (0.0, 2.0), [230, 60, 60, 255]);
            rect(-2.2, (-2.5, -1.0), (0.3, 1.8), [40, 90, 230, 128]);
            // the ground, y = 0, out to 30 m, in a checker
            if dir[1] < -1e-6 {
                let t = -eye[1] / dir[1];
                let (px, pz) = (eye[0] + t * dir[0], eye[2] + t * dir[2]);
                if t < 30.0 {
                    let on = ((px * 2.0).floor() as i32 + (pz * 2.0).floor() as i32) & 1 == 0;
                    consider(
                        t,
                        if on {
                            [70, 160, 70, 255]
                        } else {
                            [40, 110, 40, 255]
                        },
                    );
                }
            }
            if let Some((t, c)) = best {
                let i = (y * w + x) as usize;
                rgba[i * 4..i * 4 + 4].copy_from_slice(&c);
                depth[i] = t;
            }
        }
    }
    if !with_depth {
        depth.fill(f32::INFINITY);
    }
    (rgba, depth)
}

/// The host's image and linear depth: a wall at 3.4 m on the left, a ramp from 1.5 m to 9 m across the
/// middle, a pillar at 2 m, and nothing (depth `+inf`, a cleared buffer) elsewhere. Half-transparent
/// pixels (alpha 200) in one corner exercise the alpha channel.
fn render_host((w, h): (u32, u32)) -> (Vec<u8>, Vec<f32>) {
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let mut t = vec![f32::INFINITY; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) as usize;
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let mut c = [(fx * 200.0) as u8 + 20, (fy * 200.0) as u8 + 20, 90, 255];
            if fx < 0.22 && fy > 0.2 {
                t[i] = 3.4;
                c = [200, 190, 60, 255];
            } else if fx > 0.3 && fx < 0.9 && fy > 0.55 {
                t[i] = 1.5 + (fx - 0.3) / 0.6 * 7.5;
                c = [150, 90 + (fx * 100.0) as u8, 160, 255];
            } else if fx > 0.55 && fx < 0.62 && fy < 0.7 {
                t[i] = 2.0;
                c = [220, 220, 230, 255];
            }
            if x < 6 && y < 6 {
                c[3] = 200;
            }
            rgba[i * 4..i * 4 + 4].copy_from_slice(&c);
        }
    }
    (rgba, t)
}

/// Host linear depth -> the reversed-Z buffer values for `cam`.
fn to_buffer(t: &[f32], cam: &Camera) -> Vec<f32> {
    t.iter()
        .map(|&t| delinearize_depth(t, cam.near, cam.far, cam.depth_range))
        .collect()
}

fn frame(rgba: Vec<u8>, depth: Option<Vec<f32>>, size: (u32, u32)) -> GuestFrame {
    let mut f = GuestFrame::new(1, 0, 0, size, rgba, depth).unwrap();
    f.id = 1;
    f
}

// ---------------------------------------------------------------------------------------------
// Comparing

#[derive(Debug, Default)]
struct Diff {
    pixels: usize,
    /// Pixels where any channel differs.
    differing: usize,
    /// Pixels where any channel differs by more than 1.
    over_one: usize,
    max: u8,
    /// The first few differing pixels, for the message: (x, y, cpu, gpu)
    first: Vec<(u32, u32, [u8; 4], [u8; 4])>,
}

fn diff(cpu: &[u8], gpu: &[u8], w: u32) -> Diff {
    let mut d = Diff {
        pixels: cpu.len() / 4,
        ..Default::default()
    };
    for (i, (a, b)) in cpu.chunks(4).zip(gpu.chunks(4)).enumerate() {
        let m = (0..4).map(|c| a[c].abs_diff(b[c])).max().unwrap();
        if m > 0 {
            d.differing += 1;
            if d.first.len() < 6 {
                d.first.push((
                    i as u32 % w,
                    i as u32 / w,
                    [a[0], a[1], a[2], a[3]],
                    [b[0], b[1], b[2], b[3]],
                ));
            }
        }
        if m > 1 {
            d.over_one += 1;
        }
        d.max = d.max.max(m);
    }
    d
}

/// Whether pixel `i` of `a` differs from the image `b` in a 3 x 3 neighbourhood of the same pixel,
/// meaning the pixel sits at an edge of the CPU's own result: a one-pixel shift of an edge changes
/// it.
fn at_edge(a: &[u8], i: usize, w: u32, h: u32) -> bool {
    let (x, y) = ((i as u32 % w) as i32, (i as u32 / w) as i32);
    for dy in -1..=1 {
        for dx in -1..=1 {
            let (nx, ny) = (x + dx, y + dy);
            if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                continue;
            }
            let j = (ny as u32 * w + nx as u32) as usize;
            if a[j * 4..j * 4 + 4] != a[i * 4..i * 4 + 4] {
                return true;
            }
        }
    }
    false
}

/// The tolerance for reprojected results: every differing pixel is on an edge of the CPU image (a
/// footprint boundary moved by a rounding error), and the share of differing pixels is below
/// `max_share`.
fn compare_warp(name: &str, cpu: &[u8], gpu: &[u8], size: (u32, u32), max_share: f64) -> Diff {
    let d = diff(cpu, gpu, size.0);
    let share = d.differing as f64 / d.pixels as f64;
    eprintln!(
        "{name}: {} px, {} differ ({:.3}%), {} by more than 1, max channel diff {}",
        d.pixels,
        d.differing,
        share * 100.0,
        d.over_one,
        d.max
    );
    let off_edge = cpu
        .chunks(4)
        .zip(gpu.chunks(4))
        .enumerate()
        .filter(|(i, (a, b))| {
            a != b && !at_edge(cpu, *i, size.0, size.1) && !at_edge(gpu, *i, size.0, size.1)
        })
        .count();
    assert_eq!(
        off_edge, 0,
        "{name}: pixels that differ away from any edge: {:?}",
        d.first
    );
    assert!(
        share <= max_share,
        "{name}: {:.3}% of the pixels differ (limit {:.3}%): {:?}",
        share * 100.0,
        max_share * 100.0,
        d.first
    );
    d
}

/// The CPU result: reproject (or not) and composite.
fn cpu_result(
    host_rgba: &[u8],
    host_t: &[f32],
    guest: &GuestFrame,
    render: &Camera,
    present: &Camera,
    host_size: (u32, u32),
    warp: bool,
) -> Vec<u8> {
    let depth: Vec<f32> = match &guest.depth {
        Some(d) => d.to_vec(),
        // no depth plane: the reference pastes; the stand-in is in front of everything
        None => vec![1.0; (guest.width * guest.height) as usize],
    };
    let (g_rgba, g_depth) = if warp {
        reproject(
            &guest.rgba,
            &depth,
            guest.width,
            guest.height,
            render,
            present,
            host_size.0,
            host_size.1,
        )
        .unwrap()
    } else {
        (guest.rgba.to_vec(), depth)
    };
    composite(host_rgba, host_t, &g_rgba, &g_depth)
        .unwrap()
        .rgba
}

fn params(
    guest: &GuestFrame,
    host_size: (u32, u32),
    present: &Camera,
    render: Option<&Camera>,
    srgb: bool,
) -> ComposeParams {
    ComposeParams {
        guest_size: (guest.width, guest.height),
        dst_size: host_size,
        origin: (0, 0),
        has_depth: guest.depth.is_some(),
        srgb,
        host_depth: host_depth_terms(&present.proj).unwrap(),
        warp: render.map(|r| WarpMatrices::new(r, &present.view, &present.proj).unwrap()),
    }
}

const SIZE: (u32, u32) = (160, 120);

/// The share of pixels of a reprojected composite that may differ between CPU and GPU: 0.05%, 9 pixels of
/// the 19200 here. Measured on the development machine (Apple GPU, Metal) it is 0.
const MAX_WARP_DIFF_SHARE: f64 = 0.0005;

// ---------------------------------------------------------------------------------------------
// Tests

#[test]
fn host_depth_terms_match_the_references_linearize_in_every_form() {
    for far in [100.0, f32::INFINITY] {
        let cam = camera([0.0, 0.0, 0.0], [0.0, 0.0, -1.0], SIZE, far);
        let (a, b) = host_depth_terms(&cam.proj).unwrap();
        for t in [0.11f32, 0.5, 1.0, 3.0, 20.0, 90.0] {
            let d = delinearize_depth(t, cam.near, cam.far, cam.depth_range);
            let got = b / (d + a);
            assert!(((got - t) / t).abs() < 2e-4, "far {far}, t {t}: {got}");
        }
    }
    // an orthographic projection has none
    let mut cam = camera([0.0; 3], [0.0, 0.0, -1.0], SIZE, 100.0);
    cam.proj[11] = 0.0;
    cam.proj[15] = 1.0;
    assert!(host_depth_terms(&cam.proj).is_none());
}

#[test]
fn composite_without_a_warp_matches_the_reference_exactly() {
    let (device, queue) = gpu_or_skip!();
    for far in [f32::INFINITY, 200.0] {
        let cam = camera([0.0, 1.5, 2.0], [0.0, 1.0, -4.0], SIZE, far);
        let (g_rgba, g_depth) = render_guest(&cam, SIZE, true);
        let (h_rgba, h_t) = render_host(SIZE);
        let guest = frame(g_rgba, Some(g_depth), SIZE);
        let d = to_buffer(&h_t, &cam);
        // the reference sees the depth buffer the way the host would: converted with its camera
        let t_from_buffer = linearize_buffer(&d, &cam);
        let cpu = cpu_result(&h_rgba, &t_from_buffer, &guest, &cam, &cam, SIZE, false);
        let gpu_out = run_gpu(
            device,
            queue,
            &Run {
                host: Host {
                    rgba: &h_rgba,
                    d: &d,
                    size: SIZE,
                    format: TextureFormat::Rgba8Unorm,
                },
                guest: &guest,
                params: params(&guest, SIZE, &cam, None, false),
            },
        );
        let df = diff(&cpu, &gpu_out, SIZE.0);
        eprintln!(
            "composite, far {far}: {} px, {} differ, max channel diff {}",
            df.pixels, df.differing, df.max
        );
        // both occlusion directions really happen in this scene
        let reference = composite(
            &h_rgba,
            &t_from_buffer,
            &guest.rgba,
            guest.depth.as_ref().unwrap(),
        )
        .unwrap();
        assert!(reference.guest_in_front > 500 && reference.guest_hidden > 500);
        assert_eq!(df.differing, 0, "{:?}", df.first);
    }
}

#[test]
fn composite_through_an_srgb_target_is_within_one_step() {
    let (device, queue) = gpu_or_skip!();
    let cam = camera([0.0, 1.5, 2.0], [0.0, 1.0, -4.0], SIZE, f32::INFINITY);
    let (g_rgba, g_depth) = render_guest(&cam, SIZE, true);
    let (h_rgba, h_t) = render_host(SIZE);
    let guest = frame(g_rgba, Some(g_depth), SIZE);
    let d = to_buffer(&h_t, &cam);
    let t_from_buffer = linearize_buffer(&d, &cam);
    let cpu = cpu_result(&h_rgba, &t_from_buffer, &guest, &cam, &cam, SIZE, false);
    let gpu_out = run_gpu(
        device,
        queue,
        &Run {
            host: Host {
                rgba: &h_rgba,
                d: &d,
                size: SIZE,
                format: TextureFormat::Rgba8UnormSrgb,
            },
            guest: &guest,
            params: params(&guest, SIZE, &cam, None, true),
        },
    );
    let df = diff(&cpu, &gpu_out, SIZE.0);
    eprintln!(
        "composite, sRGB target: {} px, {} differ, {} by more than 1, max channel diff {}",
        df.pixels, df.differing, df.over_one, df.max
    );
    assert_eq!(df.over_one, 0, "{:?}", df.first);
}

#[test]
fn a_frame_without_depth_is_pasted_over_the_host() {
    let (device, queue) = gpu_or_skip!();
    let cam = camera([0.0, 1.5, 2.0], [0.0, 1.0, -4.0], SIZE, f32::INFINITY);
    let (g_rgba, _) = render_guest(&cam, SIZE, false);
    let (h_rgba, h_t) = render_host(SIZE);
    let guest = frame(g_rgba, None, SIZE);
    let d = to_buffer(&h_t, &cam);
    let t_from_buffer = linearize_buffer(&d, &cam);
    let cpu = cpu_result(&h_rgba, &t_from_buffer, &guest, &cam, &cam, SIZE, false);
    let gpu_out = run_gpu(
        device,
        queue,
        &Run {
            host: Host {
                rgba: &h_rgba,
                d: &d,
                size: SIZE,
                format: TextureFormat::Rgba8Unorm,
            },
            guest: &guest,
            params: params(&guest, SIZE, &cam, None, false),
        },
    );
    // the host's wall is nearer than the sphere, and the pasted guest still covers it
    let df = diff(&cpu, &gpu_out, SIZE.0);
    assert_eq!(df.differing, 0, "{:?}", df.first);
}

#[test]
fn reprojection_alone_matches_the_reference() {
    let (device, queue) = gpu_or_skip!();
    let render = camera([0.0, 1.5, 2.0], [0.0, 1.0, -4.0], SIZE, f32::INFINITY);
    let present = camera([0.35, 1.5, 2.0], [0.2, 1.0, -4.0], SIZE, f32::INFINITY);
    let (g_rgba, g_depth) = render_guest(&render, SIZE, true);
    let guest = frame(g_rgba, Some(g_depth), SIZE);
    // an empty host: every guest pixel that lands shows
    let h_rgba = vec![0u8; (SIZE.0 * SIZE.1 * 4) as usize];
    let h_t = vec![f32::INFINITY; (SIZE.0 * SIZE.1) as usize];
    let d = to_buffer(&h_t, &present);
    let cpu = cpu_result(&h_rgba, &h_t, &guest, &render, &present, SIZE, true);
    let gpu_out = run_gpu(
        device,
        queue,
        &Run {
            host: Host {
                rgba: &h_rgba,
                d: &d,
                size: SIZE,
                format: TextureFormat::Rgba8Unorm,
            },
            guest: &guest,
            params: params(&guest, SIZE, &present, Some(&render), false),
        },
    );
    let covered = cpu.chunks(4).filter(|p| p[3] != 0).count();
    assert!(
        covered > 5000,
        "the warp should land many pixels, got {covered}"
    );
    // the warp actually moved something
    let unwarped = cpu_result(&h_rgba, &h_t, &guest, &render, &render, SIZE, false);
    assert_ne!(unwarped, cpu);
    compare_warp("warp only", &cpu, &gpu_out, SIZE, MAX_WARP_DIFF_SHARE);
}

#[test]
fn reprojection_with_occlusion_matches_the_reference() {
    let (device, queue) = gpu_or_skip!();
    let render = camera([0.0, 1.5, 2.0], [0.0, 1.0, -4.0], SIZE, f32::INFINITY);
    for (name, present) in [
        (
            "sideways",
            camera([0.35, 1.5, 2.0], [0.2, 1.0, -4.0], SIZE, f32::INFINITY),
        ),
        (
            "forward and turned",
            camera([-0.2, 1.4, 1.2], [0.6, 1.1, -4.0], SIZE, f32::INFINITY),
        ),
        (
            // 1.4 m forward and down: the ground and the near pane stretch
            "lunge towards the ground",
            camera([0.0, 0.7, 0.6], [0.0, 0.8, -4.0], SIZE, f32::INFINITY),
        ),
        (
            "with a finite far plane",
            camera([0.1, 1.55, 2.2], [-0.1, 1.0, -4.0], SIZE, 150.0),
        ),
    ] {
        let (g_rgba, g_depth) = render_guest(&render, SIZE, true);
        let guest = frame(g_rgba, Some(g_depth), SIZE);
        let (h_rgba, h_t) = render_host(SIZE);
        let d = to_buffer(&h_t, &present);
        let t_from_buffer = linearize_buffer(&d, &present);
        let cpu = cpu_result(
            &h_rgba,
            &t_from_buffer,
            &guest,
            &render,
            &present,
            SIZE,
            true,
        );
        let gpu_out = run_gpu(
            device,
            queue,
            &Run {
                host: Host {
                    rgba: &h_rgba,
                    d: &d,
                    size: SIZE,
                    format: TextureFormat::Rgba8Unorm,
                },
                guest: &guest,
                params: params(&guest, SIZE, &present, Some(&render), false),
            },
        );
        compare_warp(name, &cpu, &gpu_out, SIZE, MAX_WARP_DIFF_SHARE);
    }
}

#[test]
fn a_frame_of_another_size_is_resampled_like_the_reference() {
    let (device, queue) = gpu_or_skip!();
    let host_size = SIZE;
    let present = camera([0.0, 1.5, 2.0], [0.0, 1.0, -4.0], host_size, f32::INFINITY);
    // 1.7 times smaller; 5 times (every footprint is 5 pixels across); 20 times (past the 16 pixel
    // limit, so every pixel falls back to a single destination pixel)
    for guest_size in [(96, 72), (32, 24), (8, 6)] {
        let guest_cam = camera([0.0, 1.5, 2.0], [0.0, 1.0, -4.0], guest_size, f32::INFINITY);
        let (g_rgba, g_depth) = render_guest(&guest_cam, guest_size, true);
        let guest = frame(g_rgba, Some(g_depth), guest_size);
        let (h_rgba, h_t) = render_host(host_size);
        let d = to_buffer(&h_t, &present);
        let t_from_buffer = linearize_buffer(&d, &present);
        // render camera = present camera (the unmatched-frame rule)
        let cpu = cpu_result(
            &h_rgba,
            &t_from_buffer,
            &guest,
            &present,
            &present,
            host_size,
            true,
        );
        let gpu_out = run_gpu(
            device,
            queue,
            &Run {
                host: Host {
                    rgba: &h_rgba,
                    d: &d,
                    size: host_size,
                    format: TextureFormat::Rgba8Unorm,
                },
                guest: &guest,
                params: params(&guest, host_size, &present, Some(&present), false),
            },
        );
        compare_warp(
            &format!("resample from {}x{}", guest_size.0, guest_size.1),
            &cpu,
            &gpu_out,
            host_size,
            MAX_WARP_DIFF_SHARE,
        );
    }
}

#[test]
fn a_late_frame_with_the_cameras_equal_is_the_identity() {
    // The warp with render camera = present camera and the same size gives the frame back (the
    // reference's own test says so); the GPU has to agree, so the shortcut it takes when the
    // cameras are bit-equal is not a different answer.
    let (device, queue) = gpu_or_skip!();
    let cam = camera([0.0, 1.5, 2.0], [0.0, 1.0, -4.0], SIZE, f32::INFINITY);
    let (g_rgba, g_depth) = render_guest(&cam, SIZE, true);
    let guest = frame(g_rgba, Some(g_depth), SIZE);
    let (h_rgba, h_t) = render_host(SIZE);
    let d = to_buffer(&h_t, &cam);
    let run = |warp: bool| {
        run_gpu(
            device,
            queue,
            &Run {
                host: Host {
                    rgba: &h_rgba,
                    d: &d,
                    size: SIZE,
                    format: TextureFormat::Rgba8Unorm,
                },
                guest: &guest,
                params: params(&guest, SIZE, &cam, warp.then_some(&cam), false),
            },
        )
    };
    let (direct, warped) = (run(false), run(true));
    let df = diff(&direct, &warped, SIZE.0);
    eprintln!(
        "identity warp vs direct: {} differ of {}",
        df.differing, df.pixels
    );
    assert_eq!(df.differing, 0, "{:?}", df.first);
}

#[test]
fn a_viewport_inside_a_larger_target_leaves_the_rest_alone() {
    let (device, queue) = gpu_or_skip!();
    // host target 160 x 120, composite into the 100 x 80 viewport at (40, 20)
    let vp = (100, 80);
    let cam = camera([0.0, 1.5, 2.0], [0.0, 1.0, -4.0], vp, f32::INFINITY);
    let (g_rgba, g_depth) = render_guest(&cam, vp, true);
    let guest = frame(g_rgba, Some(g_depth), vp);
    let (h_rgba, h_t) = render_host(SIZE);
    let d = to_buffer(&h_t, &cam);
    let mut p = params(&guest, vp, &cam, None, false);
    p.origin = (40, 20);
    let out = run_gpu(
        device,
        queue,
        &Run {
            host: Host {
                rgba: &h_rgba,
                d: &d,
                size: SIZE,
                format: TextureFormat::Rgba8Unorm,
            },
            guest: &guest,
            params: p,
        },
    );
    // outside the viewport: the host, untouched
    for y in 0..SIZE.1 {
        for x in 0..SIZE.0 {
            let inside = (40..140).contains(&x) && (20..100).contains(&y);
            if !inside {
                let i = ((y * SIZE.0 + x) * 4) as usize;
                assert_eq!(out[i..i + 4], h_rgba[i..i + 4], "pixel ({x}, {y})");
            }
        }
    }
    // inside: the reference, with the host's pixels and depths cut to the viewport
    let (mut hv, mut tv) = (Vec::new(), Vec::new());
    for y in 20..100u32 {
        for x in 40..140u32 {
            let i = (y * SIZE.0 + x) as usize;
            hv.extend_from_slice(&h_rgba[i * 4..i * 4 + 4]);
            tv.push(linearize_buffer(&d[i..=i], &cam)[0]);
        }
    }
    let cpu = cpu_result(&hv, &tv, &guest, &cam, &cam, vp, false);
    let mut gpu_in = Vec::new();
    for y in 20..100u32 {
        for x in 40..140u32 {
            let i = ((y * SIZE.0 + x) * 4) as usize;
            gpu_in.extend_from_slice(&out[i..i + 4]);
        }
    }
    let df = diff(&cpu, &gpu_in, vp.0);
    assert_eq!(df.differing, 0, "{:?}", df.first);
}

/// Frame times of the GPU path, upload included, waiting for the GPU to finish each frame. Not a
/// test of anything: run it with
/// `cargo test --release -p rewrite-bridge-bevy --features compose --lib frame_times -- --ignored --nocapture`.
#[test]
#[ignore = "a benchmark"]
fn frame_times() {
    use std::time::Instant;

    let (device, queue) = gpu_or_skip!();
    for (host_size, guest_size) in [
        ((1280, 720), (640, 360)),
        ((1920, 1080), (640, 360)),
        ((1920, 1080), (1280, 720)),
        ((2560, 1440), (960, 540)),
    ] {
        let render = camera([0.0, 1.5, 2.0], [0.0, 1.0, -4.0], guest_size, f32::INFINITY);
        let present = camera([0.35, 1.5, 2.0], [0.2, 1.0, -4.0], host_size, f32::INFINITY);
        let (g_rgba, g_depth) = render_guest(&render, guest_size, true);
        let guest_frame = frame(g_rgba, Some(g_depth), guest_size);
        let (h_rgba, h_t) = render_host(host_size);
        let d = to_buffer(&h_t, &present);

        let mut engine = Engine::new(device);
        let format = TextureFormat::Rgba8UnormSrgb;
        engine.prepare_format(device, format);
        let mut guest = GuestGpu::new(device, guest_size, true);
        let host_tex = texture(
            device,
            host_size,
            format,
            TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
        );
        write(queue, &host_tex, host_size, &h_rgba);
        let host_view = host_tex.create_view(&TextureViewDescriptor::default());
        let d_src = texture(
            device,
            host_size,
            TextureFormat::R32Float,
            TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
        );
        write(queue, &d_src, host_size, bytemuck::cast_slice(&d));
        let depth_tex = texture(
            device,
            host_size,
            TextureFormat::Depth32Float,
            TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT,
        );
        let depth_view = depth_tex.create_view(&TextureViewDescriptor::default());
        let target = texture(
            device,
            host_size,
            format,
            TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
        );
        let target_view = target.create_view(&TextureViewDescriptor::default());
        let mut encoder = Engine::new_encoder(device);
        fill_depth(device, &mut encoder, &d_src, &depth_view);
        queue.submit([encoder.finish()]);

        for warp in [false, true] {
            let mut p = params(
                &guest_frame,
                host_size,
                &present,
                warp.then_some(&render),
                true,
            );
            if !warp && guest_size != host_size {
                // without a warp the frame has to be the viewport's size; resample instead
                p.warp = WarpMatrices::new(&present, &present.view, &present.proj);
            }
            let buffers = p
                .warp
                .is_some()
                .then(|| WarpBuffers::new(device, guest_size, host_size));
            let mut times = Vec::new();
            for i in 0..70 {
                let began = Instant::now();
                guest.upload(queue, &guest_frame);
                let mut encoder = Engine::new_encoder(device);
                if let Some(b) = &buffers {
                    engine.encode_warp(device, &mut encoder, &p, &guest, b);
                }
                engine.encode_composite(
                    device,
                    &mut encoder,
                    &p,
                    &guest,
                    buffers.as_ref(),
                    &host_view,
                    &depth_view,
                    &target_view,
                    format,
                );
                queue.submit([encoder.finish()]);
                device.poll(PollType::wait_indefinitely()).unwrap();
                if i >= 10 {
                    times.push(began.elapsed().as_secs_f64() * 1000.0);
                }
            }
            times.sort_by(f64::total_cmp);
            let mean = times.iter().sum::<f64>() / times.len() as f64;
            eprintln!(
                "host {}x{}, guest {}x{}, {}: mean {:.2} ms, median {:.2} ms, worst {:.2} ms",
                host_size.0,
                host_size.1,
                guest_size.0,
                guest_size.1,
                if warp {
                    "reproject + composite"
                } else {
                    "resample + composite"
                },
                mean,
                times[times.len() / 2],
                times[times.len() - 1],
            );
        }
    }

    // The CPU reference for one of the sizes
    let (host_size, guest_size) = ((1280, 720), (640, 360));
    let render = camera([0.0, 1.5, 2.0], [0.0, 1.0, -4.0], guest_size, f32::INFINITY);
    let present = camera([0.35, 1.5, 2.0], [0.2, 1.0, -4.0], host_size, f32::INFINITY);
    let (g_rgba, g_depth) = render_guest(&render, guest_size, true);
    let guest_frame = frame(g_rgba, Some(g_depth), guest_size);
    let (h_rgba, h_t) = render_host(host_size);
    let began = std::time::Instant::now();
    let _ = cpu_result(
        &h_rgba,
        &h_t,
        &guest_frame,
        &render,
        &present,
        host_size,
        true,
    );
    eprintln!(
        "CPU reference (compose.rs), host 1280x720, guest 640x360, reproject + composite: {:.0} ms",
        began.elapsed().as_secs_f64() * 1000.0
    );
}
