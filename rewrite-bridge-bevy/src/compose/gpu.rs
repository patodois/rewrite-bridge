//! The GPU half of depth compositing: textures for a guest frame, the reprojection passes and the
//! compositing pass, written against `wgpu` through Bevy's `RenderDevice`, so tests can run it
//! without an app. The shaders (`common.wgsl`, `warp.wgsl`, `composite.wgsl`) follow
//! `rewrite_bridge::link::compose` rule by rule; see their headers.

use std::collections::HashMap;

use bevy::math::DMat4;
use bevy::render::render_resource::{
    BindGroupEntry, BindGroupLayout, BindGroupLayoutEntry, BindingResource, BindingType, Buffer,
    BufferBinding, BufferBindingType, BufferDescriptor, BufferInitDescriptor, BufferUsages,
    ColorTargetState, ColorWrites, CommandEncoder, CommandEncoderDescriptor, ComputePassDescriptor,
    ComputePipeline, Extent3d, LoadOp, Operations, Origin3d, PipelineLayoutDescriptor,
    PrimitiveState, RawComputePipelineDescriptor, RawFragmentState, RawRenderPipelineDescriptor,
    RawVertexState, RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline,
    ShaderModuleDescriptor, ShaderSource, ShaderStages, StoreOp, TexelCopyBufferLayout,
    TexelCopyTextureInfo, Texture, TextureAspect, TextureDescriptor, TextureDimension,
    TextureFormat, TextureSampleType, TextureUsages, TextureView, TextureViewDescriptor,
    TextureViewDimension,
};
use bevy::render::renderer::{RenderDevice, RenderQueue};
use rewrite_bridge::link::Camera;

use super::GuestFrame;

const COMMON: &str = include_str!("common.wgsl");
const WARP: &str = include_str!("warp.wgsl");
const COMPOSITE: &str = include_str!("composite.wgsl");

/// Bytes of the `Params` uniform in the shaders.
const PARAMS_LEN: usize = 16 * 3 + 16 * 12;

const F_WARP: u32 = 1;
const F_DEPTH: u32 = 2;
const F_SRGB: u32 = 4;

/// Workgroups are 8 x 8 threads.
const GROUP: u32 = 8;

// ---------------------------------------------------------------------------------------------
// Matrices

/// The three matrices of a reprojection, as the shaders want them (row by row, `f32`), computed in
/// `f64` like the reference does.
#[derive(Clone, Debug, PartialEq)]
pub struct WarpMatrices {
    /// Rows of the render camera's projection.
    pub proj: [[f32; 4]; 4],
    /// Rows of render view space to present view space.
    pub rel: [[f32; 4]; 4],
    /// Rows of render view space to present clip space.
    pub clip: [[f32; 4]; 4],
}

fn dmat(m: &[f32; 16]) -> DMat4 {
    DMat4::from_cols_array(&m.map(f64::from))
}

fn rows(m: &DMat4) -> [[f32; 4]; 4] {
    std::array::from_fn(|r| std::array::from_fn(|c| m.col(c)[r] as f32))
}

impl WarpMatrices {
    /// The matrices that carry a pixel rendered with `render` into the view `present_view` (world
    /// to view) and `present_proj` (view to clip), both column-major as on the wire. `None` for a
    /// singular render view, which the reference refuses too.
    pub fn new(
        render: &Camera,
        present_view: &[f32; 16],
        present_proj: &[f32; 16],
    ) -> Option<Self> {
        Self::from_matrices(&render.view, &render.proj, present_view, present_proj)
    }

    /// [`new`](Self::new) from the render camera's two matrices.
    pub fn from_matrices(
        render_view: &[f32; 16],
        render_proj: &[f32; 16],
        present_view: &[f32; 16],
        present_proj: &[f32; 16],
    ) -> Option<Self> {
        let inv = dmat(render_view).inverse();
        if !inv.to_cols_array().iter().all(|v| v.is_finite()) {
            return None;
        }
        let rel = dmat(present_view) * inv;
        let clip = dmat(present_proj) * rel;
        Some(Self {
            proj: rows(&dmat(render_proj)),
            rel: rows(&rel),
            clip: rows(&clip),
        })
    }
}

/// What one composite needs besides the textures.
#[derive(Clone, Debug)]
pub struct ComposeParams {
    /// The guest frame's size in pixels.
    pub guest_size: (u32, u32),
    /// The host viewport's size in pixels: the size of the image the guest is composited into.
    pub dst_size: (u32, u32),
    /// Where the viewport starts in the host target.
    pub origin: (u32, u32),
    /// The frame has a depth plane, so the depth test applies; without one the guest is pasted.
    pub has_depth: bool,
    /// The host target stores linear values of sRGB-encoded colour (an `*Srgb` format): the shader
    /// converts to encoded values to blend, as the contract's reference does, and back.
    pub srgb: bool,
    /// `(a, b)` of the host depth buffer: linear depth `t = b / (d + a)`. For the reversed,
    /// infinite projection Bevy uses, `(0, near)`.
    pub host_depth: (f32, f32),
    /// `Some` to reproject the frame first; `None` to composite it as it is (same size as the
    /// viewport).
    pub warp: Option<WarpMatrices>,
}

impl ComposeParams {
    fn bytes(&self) -> [u8; PARAMS_LEN] {
        let mut out = [0u8; PARAMS_LEN];
        let mut at = 0;
        let mut put = |v: [u8; 4]| {
            out[at..at + 4].copy_from_slice(&v);
            at += 4;
        };
        let flags = (u32::from(self.warp.is_some()) * F_WARP)
            | (u32::from(self.has_depth) * F_DEPTH)
            | (u32::from(self.srgb) * F_SRGB);
        for v in [
            self.guest_size.0,
            self.guest_size.1,
            self.dst_size.0,
            self.dst_size.1,
            self.origin.0,
            self.origin.1,
            flags,
            0,
        ] {
            put(v.to_le_bytes());
        }
        for v in [self.host_depth.0, self.host_depth.1, 0.0, 0.0] {
            put(v.to_le_bytes());
        }
        let zero = [[0.0f32; 4]; 4];
        let w = self.warp.as_ref();
        for m in [
            w.map_or(&zero, |w| &w.proj),
            w.map_or(&zero, |w| &w.rel),
            w.map_or(&zero, |w| &w.clip),
        ] {
            for v in m.iter().flatten() {
                put(v.to_le_bytes());
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// A guest frame on the GPU

/// A guest frame's colour and depth as textures. Colour is `Rgba8Uint` (the bytes as the pixel
/// file carries them, exact), depth `R32Float`.
pub struct GuestGpu {
    colour: Texture,
    depth: Texture,
    colour_view: TextureView,
    depth_view: TextureView,
    size: (u32, u32),
    has_depth: bool,
    /// The frame number last uploaded, if any (see [`GuestFrame::id`]).
    pub uploaded: Option<u64>,
}

impl GuestGpu {
    pub fn new(device: &RenderDevice, size: (u32, u32), has_depth: bool) -> Self {
        let make = |label: &str, w: u32, h: u32, format| {
            device.create_texture(&TextureDescriptor {
                label: Some(label),
                size: Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format,
                usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let colour = make("guest_colour", size.0, size.1, TextureFormat::Rgba8Uint);
        // Without a depth plane the shaders never read this; it only has to be bindable.
        let depth = if has_depth {
            make("guest_depth", size.0, size.1, TextureFormat::R32Float)
        } else {
            make("guest_depth_unused", 1, 1, TextureFormat::R32Float)
        };
        Self {
            colour_view: colour.create_view(&TextureViewDescriptor::default()),
            depth_view: depth.create_view(&TextureViewDescriptor::default()),
            colour,
            depth,
            size,
            has_depth,
            uploaded: None,
        }
    }

    /// Whether these textures fit `frame` (same size, same depth plane or not).
    pub fn fits(&self, frame: &GuestFrame) -> bool {
        self.size == (frame.width, frame.height) && self.has_depth == frame.depth.is_some()
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn has_depth(&self) -> bool {
        self.has_depth
    }

    /// Copies `frame` into the textures. `frame` must [`fit`](Self::fits).
    pub fn upload(&mut self, queue: &RenderQueue, frame: &GuestFrame) {
        debug_assert!(self.fits(frame));
        let (w, h) = self.size;
        let write = |texture: &Texture, data: &[u8], bytes_per_pixel: u32| {
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
                    bytes_per_row: Some(w * bytes_per_pixel),
                    rows_per_image: Some(h),
                },
                Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        };
        write(&self.colour, &frame.rgba, 4);
        if let Some(depth) = &frame.depth {
            write(&self.depth, bytemuck::cast_slice(depth), 4);
        }
        self.uploaded = Some(frame.id);
    }
}

// ---------------------------------------------------------------------------------------------
// Reprojection buffers

/// The buffers the reprojection passes work in, sized for one guest frame and one host viewport.
pub struct WarpBuffers {
    guest_pixels: u64,
    dst_pixels: u64,
    depth_bits: Buffer,
    index: Buffer,
    footprints: Buffer,
}

impl WarpBuffers {
    pub fn new(device: &RenderDevice, guest_size: (u32, u32), dst_size: (u32, u32)) -> Self {
        let guest_pixels = u64::from(guest_size.0) * u64::from(guest_size.1);
        let dst_pixels = u64::from(dst_size.0) * u64::from(dst_size.1);
        let make = |label: &str, size: u64| {
            device.create_buffer(&BufferDescriptor {
                label: Some(label),
                size: size.max(16),
                usage: BufferUsages::STORAGE,
                mapped_at_creation: false,
            })
        };
        Self {
            guest_pixels,
            dst_pixels,
            depth_bits: make("compose_depth", dst_pixels * 4),
            index: make("compose_index", dst_pixels * 4),
            footprints: make("compose_footprints", guest_pixels * 16),
        }
    }

    /// Whether the buffers are exactly the right size for these images.
    pub fn fits(&self, guest_size: (u32, u32), dst_size: (u32, u32)) -> bool {
        self.guest_pixels == u64::from(guest_size.0) * u64::from(guest_size.1)
            && self.dst_pixels == u64::from(dst_size.0) * u64::from(dst_size.1)
    }
}

// ---------------------------------------------------------------------------------------------
// Pipelines

/// The compute pipelines, the compositing pipelines (one per target format) and their layouts.
pub struct Engine {
    warp_layout: BindGroupLayout,
    composite_layout: BindGroupLayout,
    clear: ComputePipeline,
    splat_depth: ComputePipeline,
    splat_index: ComputePipeline,
    composite_module: bevy::render::render_resource::ShaderModule,
    composite_pipeline_layout: bevy::render::render_resource::PipelineLayout,
    composite: HashMap<TextureFormat, RenderPipeline>,
    /// Bound where the warped buffers are not used.
    dummy: Buffer,
}

fn entry(binding: u32, visibility: ShaderStages, ty: BindingType) -> BindGroupLayoutEntry {
    BindGroupLayoutEntry {
        binding,
        visibility,
        ty,
        count: None,
    }
}

fn uniform(binding: u32, visibility: ShaderStages) -> BindGroupLayoutEntry {
    entry(
        binding,
        visibility,
        BindingType::Buffer {
            ty: BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
    )
}

fn storage(binding: u32, visibility: ShaderStages, read_only: bool) -> BindGroupLayoutEntry {
    entry(
        binding,
        visibility,
        BindingType::Buffer {
            ty: BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
    )
}

fn texture(
    binding: u32,
    visibility: ShaderStages,
    sample_type: TextureSampleType,
) -> BindGroupLayoutEntry {
    entry(
        binding,
        visibility,
        BindingType::Texture {
            sample_type,
            view_dimension: TextureViewDimension::D2,
            multisampled: false,
        },
    )
}

const UNFILTERED: TextureSampleType = TextureSampleType::Float { filterable: false };

impl Engine {
    pub fn new(device: &RenderDevice) -> Self {
        let c = ShaderStages::COMPUTE;
        let f = ShaderStages::FRAGMENT;
        let warp_layout = device.create_bind_group_layout(
            "compose_warp_layout",
            &[
                uniform(0, c),
                texture(1, c, TextureSampleType::Uint),
                texture(2, c, UNFILTERED),
                storage(3, c, false),
                storage(4, c, false),
                storage(5, c, false),
            ],
        );
        let composite_layout = device.create_bind_group_layout(
            "compose_composite_layout",
            &[
                uniform(0, f),
                texture(1, f, UNFILTERED),
                texture(2, f, TextureSampleType::Depth),
                texture(3, f, TextureSampleType::Uint),
                texture(4, f, UNFILTERED),
                storage(5, f, true),
                storage(6, f, true),
            ],
        );

        let warp_source = format!("{COMMON}\n{WARP}");
        let warp_module = device.create_and_validate_shader_module(ShaderModuleDescriptor {
            label: Some("compose_warp"),
            source: ShaderSource::Wgsl(warp_source.into()),
        });
        let warp_pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("compose_warp_pipeline_layout"),
            bind_group_layouts: &[Some(&warp_layout)],
            immediate_size: 0,
        });
        let compute = |entry_point: &str| {
            device.create_compute_pipeline(&RawComputePipelineDescriptor {
                label: Some(entry_point),
                layout: Some(&warp_pipeline_layout),
                module: &warp_module,
                entry_point: Some(entry_point),
                compilation_options: Default::default(),
                cache: None,
            })
        };

        let composite_source = format!("{COMMON}\n{COMPOSITE}");
        let composite_module = device.create_and_validate_shader_module(ShaderModuleDescriptor {
            label: Some("compose_composite"),
            source: ShaderSource::Wgsl(composite_source.into()),
        });
        let composite_pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("compose_composite_pipeline_layout"),
            bind_group_layouts: &[Some(&composite_layout)],
            immediate_size: 0,
        });

        Self {
            clear: compute("clear"),
            splat_depth: compute("splat_depth"),
            splat_index: compute("splat_index"),
            dummy: device.create_buffer(&BufferDescriptor {
                label: Some("compose_dummy"),
                size: 16,
                usage: BufferUsages::STORAGE,
                mapped_at_creation: false,
            }),
            warp_layout,
            composite_layout,
            composite_module,
            composite_pipeline_layout,
            composite: HashMap::new(),
        }
    }

    /// Whether the compositing pipeline for targets of `format` exists.
    pub fn has_format(&self, format: TextureFormat) -> bool {
        self.composite.contains_key(&format)
    }

    /// Makes sure the compositing pipeline for targets of `format` exists.
    pub fn prepare_format(&mut self, device: &RenderDevice, format: TextureFormat) {
        if self.composite.contains_key(&format) {
            return;
        }
        let pipeline = device.create_render_pipeline(&RawRenderPipelineDescriptor {
            label: Some("compose_composite_pipeline"),
            layout: Some(&self.composite_pipeline_layout),
            vertex: RawVertexState {
                module: &self.composite_module,
                entry_point: Some("vertex"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(RawFragmentState {
                module: &self.composite_module,
                entry_point: Some("fragment"),
                compilation_options: Default::default(),
                targets: &[Some(ColorTargetState {
                    format,
                    blend: None,
                    write_mask: ColorWrites::ALL,
                })],
            }),
            primitive: PrimitiveState::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        self.composite.insert(format, pipeline);
    }

    /// Records the compositing of `guest` into `target`: the host image `host` (a view of the texture
    /// the host rendered, which must not be `target`) with `host_depth` its depth buffer, written to
    /// `target` whole. With `params.warp` set, `buffers` holds the reprojection and
    /// [`encode_warp`](Self::encode_warp) has to have been recorded before.
    ///
    /// Returns `false`, recording nothing, if the pipeline for `format` was not
    /// [prepared](Self::prepare_format).
    #[allow(clippy::too_many_arguments)]
    pub fn encode_composite(
        &self,
        device: &RenderDevice,
        encoder: &mut CommandEncoder,
        params: &ComposeParams,
        guest: &GuestGpu,
        buffers: Option<&WarpBuffers>,
        host: &TextureView,
        host_depth: &TextureView,
        target: &TextureView,
        format: TextureFormat,
    ) -> bool {
        let Some(pipeline) = self.composite.get(&format) else {
            return false;
        };
        let uniform = params_buffer(device, params);
        let (depth_bits, index) = match buffers {
            Some(b) => (&b.depth_bits, &b.index),
            None => (&self.dummy, &self.dummy),
        };
        let bind_group = device.create_bind_group(
            "compose_composite_bind_group",
            &self.composite_layout,
            &[
                BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: BindingResource::TextureView(host),
                },
                BindGroupEntry {
                    binding: 2,
                    resource: BindingResource::TextureView(host_depth),
                },
                BindGroupEntry {
                    binding: 3,
                    resource: BindingResource::TextureView(&guest.colour_view),
                },
                BindGroupEntry {
                    binding: 4,
                    resource: BindingResource::TextureView(&guest.depth_view),
                },
                BindGroupEntry {
                    binding: 5,
                    resource: whole(depth_bits),
                },
                BindGroupEntry {
                    binding: 6,
                    resource: whole(index),
                },
            ],
        );
        let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("compose_composite"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: Operations {
                    load: LoadOp::Clear(Default::default()),
                    store: StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1);
        true
    }

    /// Records the reprojection of `guest` for `params.warp` into `buffers`: clear, splat the
    /// nearest depth, then resolve ties by the lowest source index.
    pub fn encode_warp(
        &self,
        device: &RenderDevice,
        encoder: &mut CommandEncoder,
        params: &ComposeParams,
        guest: &GuestGpu,
        buffers: &WarpBuffers,
    ) {
        debug_assert!(params.warp.is_some());
        let uniform = params_buffer(device, params);
        let bind_group = device.create_bind_group(
            "compose_warp_bind_group",
            &self.warp_layout,
            &[
                BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: BindingResource::TextureView(&guest.colour_view),
                },
                BindGroupEntry {
                    binding: 2,
                    resource: BindingResource::TextureView(&guest.depth_view),
                },
                BindGroupEntry {
                    binding: 3,
                    resource: whole(&buffers.depth_bits),
                },
                BindGroupEntry {
                    binding: 4,
                    resource: whole(&buffers.index),
                },
                BindGroupEntry {
                    binding: 5,
                    resource: whole(&buffers.footprints),
                },
            ],
        );
        let groups = |(w, h): (u32, u32)| (w.div_ceil(GROUP), h.div_ceil(GROUP));
        for (label, pipeline, size) in [
            ("compose_clear", &self.clear, params.dst_size),
            ("compose_splat_depth", &self.splat_depth, params.guest_size),
            ("compose_splat_index", &self.splat_index, params.guest_size),
        ] {
            // One pass each, so every pass sees the previous one's writes.
            let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor {
                label: Some(label),
                timestamp_writes: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            let (gx, gy) = groups(size);
            pass.dispatch_workgroups(gx, gy, 1);
        }
    }

    /// A command encoder for callers (tests) that run the passes themselves.
    pub fn new_encoder(device: &RenderDevice) -> CommandEncoder {
        device.create_command_encoder(&CommandEncoderDescriptor::default())
    }
}

fn whole(buffer: &Buffer) -> BindingResource<'_> {
    BindingResource::Buffer(BufferBinding {
        buffer,
        offset: 0,
        size: None,
    })
}

fn params_buffer(device: &RenderDevice, params: &ComposeParams) -> Buffer {
    device.create_buffer_with_data(&BufferInitDescriptor {
        label: Some("compose_params"),
        contents: &params.bytes(),
        usage: BufferUsages::UNIFORM,
    })
}

// ---------------------------------------------------------------------------------------------
// Depth of the host's projection

/// `(a, b)` such that the host's depth buffer value `d` of a surface `t` metres away is
/// `d = b / t - a`, that is `t = b / (d + a)`. They are the depth row of a perspective
/// `clip_from_view` (column-major, `w_clip = -z_view`): `z_clip = a * z_view + b`, so
/// `d = z_clip / w_clip = -a + b / t`. For Bevy's reversed, infinite projection `a = 0` and
/// `b = near`. `None` for a projection that is not perspective.
pub fn host_depth_terms(proj: &[f32; 16]) -> Option<(f32, f32)> {
    let perspective = proj[3] == 0.0 && proj[7] == 0.0 && proj[11] == -1.0 && proj[15] == 0.0;
    perspective.then(|| (proj[10], proj[14]))
}
