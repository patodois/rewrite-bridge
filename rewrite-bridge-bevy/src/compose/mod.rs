//! Composite a guest's frames into a Bevy camera's image with a per-pixel depth test, and, when a frame is
//! late, reproject it to the camera the host is presenting now (contract section 9.7 of
//! [rewrite-bridge](https://github.com/patodois/rewrite-bridge)).
//!
//! This is the GPU form of the crate's reference code, `rewrite_bridge::link::compose`: the same rules, run
//! by compute and fragment shaders (`warp.wgsl`, `composite.wgsl` next to this file read like the
//! reference does), and tested against it pixel by pixel. It needs the Cargo feature `compose`.
//!
//! ```no_run
//! use bevy::prelude::*;
//! use rewrite_bridge_bevy::compose::*;
//! use rewrite_bridge::link::LinkConfig;
//!
//! fn main() {
//!     App::new()
//!         .add_plugins((
//!             DefaultPlugins,
//!             GuestComposePlugin::default(),
//!             // optional: the host side of the link (listens, sends the camera, reads the frames)
//!             HostLinkPlugin::new(LinkConfig::new("My Game")),
//!         ))
//!         .add_systems(Startup, |mut commands: Commands| {
//!             commands.spawn((
//!                 Camera3d::default(),
//!                 Msaa::Off,
//!                 GuestCompose::default(), // composite the guest's frames into this camera
//!             ));
//!         })
//!         .run();
//! }
//! ```
//!
//! # How it fits together
//!
//! ```text
//!   main world                                          render world
//!   HostLinkPlugin (or your own code)
//!     Link::send_camera(cam) -> seq ---> GuestFrames.push_camera(seq, cam)
//!     Event::Frame -> FrameReader ----> GuestFrames.set_frame(frame)
//!                                          | extract (Arc clones)
//!                                          v
//!                                       upload colour (Rgba8Uint) + depth (R32Float) textures once per frame
//!                                       per camera with GuestCompose, after the main pass and tonemapping:
//!                                         [reproject: 3 compute passes]  warp the frame from the camera
//!                                                                        named by camera_seq to this view
//!                                         [composite: 1 fullscreen pass] guest vs the camera's own depth
//! ```
//!
//! # What the host's camera has to be
//!
//! - **A perspective camera whose projection is a plain `clip_from_view`** (Bevy's default reversed,
//!   infinite or finite). An orthographic camera is left alone with a warning, since the host depth
//!   buffer cannot be turned into the linear metres the contract compares (it says the host does that
//!   with its own matrix).
//! - **`Msaa::Off`.** With MSAA the camera's depth buffer has several samples per pixel, and which to
//!   compare against is not something the contract settles.
//! - **A low-dynamic-range camera** (no [`Hdr`](bevy::camera::Hdr)): the target holds display values and
//!   the contract blends the guest's sRGB-encoded bytes with the host's. For the `*Srgb` formats Bevy
//!   uses, the shader converts the host pixel to its encoded value, blends in integers exactly as the
//!   reference does, and converts back. Tonemapping of an HDR camera happens in a node of its own, and
//!   blending there would not be the contract's blend; such a camera is skipped with a warning.
//! - Anything that does not write depth (alpha-blended materials, for instance) neither hides the guest
//!   nor is hidden by it where it is in front: only what is in the depth buffer takes part.
//!
//! [`GuestCompose`] adds the one thing Bevy needs to make the depth buffer readable
//! (`TextureUsages::TEXTURE_BINDING`) to the camera's `Camera3d` by itself.
//!
//! # What is left out
//!
//! The reference's own limits apply (splats can leave cracks where a surface is stretched a lot, and
//! shading that depends on the view is not corrected). Besides, this module does not do the following:
//!
//! - **TAA and other jitter.** The present camera is the view's unjittered `clip_from_view`.
//! - **A frame without depth into a differently sized view.** The contract pastes such a frame over the
//!   host's image; for a size that does not match it says nothing, so the frame is resampled with the
//!   reference's own rule for unmatched frames (render camera = present camera, see
//!   `rewrite_bridge::link::compose::reproject`), with a stand-in depth that is never tested.

#[cfg(all(test, feature = "viewer"))]
mod app_tests;
mod gpu;
mod host_link;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use bevy::camera::{Camera3d, Camera3dDepthTextureUsage};
use bevy::core_pipeline::schedule::{Core3d, Core3dSystems};
use bevy::core_pipeline::tonemapping::tonemapping;
use bevy::log::warn_once;
use bevy::math::{DMat4, Mat4};
use bevy::prelude::*;
use bevy::render::camera::ExtractedCamera;
use bevy::render::extract_component::{ExtractComponent, ExtractComponentPlugin};
use bevy::render::render_resource::{TextureFormat, TextureUsages};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue, ViewQuery};
use bevy::render::view::{ExtractedView, Msaa, ViewDepthStencilTexture, ViewTarget};
use bevy::render::{Extract, ExtractSchedule, Render, RenderApp, RenderStartup, RenderSystems};
use rewrite_bridge::link::compose::CameraRing;
use rewrite_bridge::link::{now_us, Camera as HostCamera, DepthRange};

pub use gpu::{host_depth_terms, ComposeParams, Engine, GuestGpu, WarpBuffers, WarpMatrices};
pub use host_link::{HostLink, HostLinkPlugin, HostLinkStats};

// ---------------------------------------------------------------------------------------------
// Data

/// One guest frame, ready to upload: the pixel file's colour and, if the file has a depth plane, the
/// linear depth in metres (contract section 9.6).
#[derive(Clone, Debug)]
pub struct GuestFrame {
    /// Counts the frames handed to [`GuestFrames::set_frame`]; it tells the uploader a frame is new
    /// even when the pixel file changed and its frame numbers started again.
    pub id: u64,
    /// The frame number in the pixel file.
    pub frame: u64,
    /// When the guest rendered it (contract section 5), microseconds.
    pub time_us: u64,
    /// The header `seq` of the host camera it was rendered with, 0 for none.
    pub camera_seq: u32,
    pub width: u32,
    pub height: u32,
    /// `width * height` straight-alpha RGBA8 pixels, sRGB encoded, rows top to bottom.
    pub rgba: Arc<Vec<u8>>,
    /// `width * height` linear depths in metres (`+inf` or any non-finite value: nothing here).
    pub depth: Option<Arc<Vec<f32>>>,
}

impl GuestFrame {
    /// A frame from its parts; `None` when the buffers do not hold `width * height` pixels, or the size is
    /// 0 or beyond the contract's limits (16384 on a side, 2^26 pixels).
    pub fn new(
        frame: u64,
        time_us: u64,
        camera_seq: u32,
        (width, height): (u32, u32),
        rgba: Vec<u8>,
        depth: Option<Vec<f32>>,
    ) -> Option<Self> {
        let n = u64::from(width) * u64::from(height);
        let ok = (1..=rewrite_bridge::link::MAX_FRAME_DIM).contains(&width)
            && (1..=rewrite_bridge::link::MAX_FRAME_DIM).contains(&height)
            && n <= rewrite_bridge::link::MAX_FRAME_PIXELS
            && rgba.len() as u64 == n * 4
            && depth.as_ref().is_none_or(|d| d.len() as u64 == n);
        ok.then(|| Self {
            id: 0,
            frame,
            time_us,
            camera_seq,
            width,
            height,
            rgba: Arc::new(rgba),
            depth: depth.map(Arc::new),
        })
    }

    /// How long ago the guest rendered the frame, by the link's clock (contract section 5).
    pub fn age(&self) -> Duration {
        Duration::from_micros(now_us().saturating_sub(self.time_us))
    }
}

/// The main-world side: the newest guest frame and the ring of cameras the host sent, so a frame's
/// `camera_seq` can be turned back into the camera it was rendered with.
#[derive(Resource, Debug)]
pub struct GuestFrames {
    cameras: CameraRing,
    frame: Option<GuestFrame>,
    next_id: u64,
}

impl Default for GuestFrames {
    fn default() -> Self {
        Self::new(CameraRing::DEFAULT_CAPACITY)
    }
}

impl GuestFrames {
    /// Keeps the last `ring_capacity` cameras (the contract wants at least half a second: 32 at 60 Hz).
    pub fn new(ring_capacity: usize) -> Self {
        Self {
            cameras: CameraRing::new(ring_capacity),
            frame: None,
            next_id: 1,
        }
    }

    /// Remembers the camera that went out in the datagram numbered `seq`. Call it with what
    /// `Link::send_camera` returned (the [`HostLinkPlugin`] does).
    pub fn push_camera(&mut self, seq: u32, camera: HostCamera) {
        self.cameras.push(seq, camera);
    }

    /// Makes `frame` the one to show, if it is newer than the current one *within its pixel file*; the
    /// caller decides that (frame numbers restart in a new file). Returns the stored frame's id.
    pub fn set_frame(&mut self, mut frame: GuestFrame) -> u64 {
        frame.id = self.next_id;
        self.next_id += 1;
        let id = frame.id;
        self.frame = Some(frame);
        id
    }

    /// Stops showing a guest (it left, or its pixel file went away).
    pub fn clear_frame(&mut self) {
        self.frame = None;
    }

    /// The frame being shown.
    pub fn frame(&self) -> Option<&GuestFrame> {
        self.frame.as_ref()
    }

    /// The camera the current frame was rendered with, if its `camera_seq` is still in the ring.
    pub fn render_camera(&self) -> Option<&HostCamera> {
        self.cameras.get(self.frame.as_ref()?.camera_seq)
    }

    /// The cameras sent so far.
    pub fn cameras(&self) -> &CameraRing {
        &self.cameras
    }
}

// ---------------------------------------------------------------------------------------------
// Cameras

/// Why a Bevy camera cannot be described to the guest as a contract [`HostCamera`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedCamera {
    /// `w_clip` is not `-z_view`: an orthographic or oblique projection.
    NotPerspective,
    /// The near plane read from the projection is not a positive number.
    BadNear,
    /// A NaN or infinity in the matrices.
    NotFinite,
}

impl std::fmt::Display for UnsupportedCamera {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotPerspective => {
                "the projection is not a perspective one (w_clip is not -z_view)"
            }
            Self::BadNear => "the near plane read from the projection is not a positive number",
            Self::NotFinite => "the camera holds a NaN or infinity",
        })
    }
}

impl std::error::Error for UnsupportedCamera {}

/// World to view as the contract wants it (column-major `f32`), inverted in `f64`. Both sides of
/// the compositing use this function, so equal cameras are equal bit for bit.
pub fn view_matrix(world_from_view: &GlobalTransform) -> [f32; 16] {
    let m = DMat4::from_cols_array(&world_from_view.to_matrix().to_cols_array().map(f64::from));
    m.inverse().to_cols_array().map(|v| v as f32)
}

/// A Bevy camera as the contract's `Camera` (section 9.3): its view, its projection (Bevy's, reversed
/// clip depth, `depth_range` 2, +y up in NDC as the contract requires), field of view, near and far,
/// and the viewport in pixels. `far` is `+inf` for the infinite projection Bevy uses by default.
pub fn host_camera(
    world_from_view: &GlobalTransform,
    clip_from_view: Mat4,
    viewport: UVec2,
) -> Result<HostCamera, UnsupportedCamera> {
    let proj = clip_from_view.to_cols_array();
    let view = view_matrix(world_from_view);
    if proj.iter().chain(&view).any(|v| !v.is_finite()) {
        return Err(UnsupportedCamera::NotFinite);
    }
    if host_depth_terms(&proj).is_none() {
        return Err(UnsupportedCamera::NotPerspective);
    }
    // Reversed depth row: z_clip = a z_view + b with (a, b) = (0, near) for an infinite far plane and
    // (n / (f - n), n f / (f - n)) for a finite one (contract section 9.7).
    let (a, b) = (f64::from(proj[10]), f64::from(proj[14]));
    let (near, far) = if a.abs() < 1e-12 {
        (b, f64::INFINITY)
    } else {
        let far = b / a;
        (a * far / (1.0 + a), far)
    };
    if !(near.is_finite() && near > 0.0) {
        return Err(UnsupportedCamera::BadNear);
    }
    Ok(HostCamera {
        view,
        proj,
        fov_y: (2.0 * (1.0 / f64::from(proj[5])).atan()) as f32,
        near: near as f32,
        far: far as f32,
        viewport: [viewport.x, viewport.y],
        depth_range: DepthRange::Reversed,
    })
}

// ---------------------------------------------------------------------------------------------
// Plugin

/// On a [`Camera3d`]: composite the guest's frames into this camera's image. The camera should be
/// `Msaa::Off` and not `Hdr` (see the module docs).
#[derive(Component, ExtractComponent, Clone, Copy, Debug, PartialEq, Eq)]
#[extract_app(RenderApp)]
pub struct GuestCompose {
    /// Reproject a late frame from the camera it was rendered with to this camera's current view (when
    /// that camera is still in the ring). With `false` the frame is composited as if it had been
    /// rendered with the current camera, which is what the contract says to do without a camera.
    pub reproject: bool,
}

impl Default for GuestCompose {
    fn default() -> Self {
        Self { reproject: true }
    }
}

/// Adds the GPU compositing, the [`GuestFrames`] resource and the extraction of both. Add it after
/// `DefaultPlugins` (it needs the render app).
#[derive(Clone, Debug)]
pub struct GuestComposePlugin {
    /// How many host cameras to remember; see [`GuestFrames::new`].
    pub ring_capacity: usize,
}

impl Default for GuestComposePlugin {
    fn default() -> Self {
        Self {
            ring_capacity: CameraRing::DEFAULT_CAPACITY,
        }
    }
}

impl Plugin for GuestComposePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GuestFrames>()
            .insert_resource(GuestFrames::new(self.ring_capacity))
            .add_plugins(ExtractComponentPlugin::<GuestCompose>::default())
            .add_systems(PostUpdate, make_depth_readable);
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app
            .init_resource::<ComposeInput>()
            .add_systems(RenderStartup, init_gpu)
            .add_systems(ExtractSchedule, extract_input)
            .add_systems(
                Render,
                (prepare_guest, prepare_views)
                    .chain()
                    .in_set(RenderSystems::Prepare),
            )
            .add_systems(
                Core3d,
                compose_pass
                    .in_set(Core3dSystems::PostProcess)
                    .after(tonemapping),
            );
    }
}

/// The depth buffer of a [`Camera3d`] is only bindable (for the compositing pass to read it) if the
/// camera asks for that.
fn make_depth_readable(mut cameras: Query<&mut Camera3d, With<GuestCompose>>) {
    for mut camera in &mut cameras {
        let usages = TextureUsages::from(camera.depth_texture_usages);
        if !usages.contains(TextureUsages::TEXTURE_BINDING) {
            camera.depth_texture_usages =
                Camera3dDepthTextureUsage::from(usages | TextureUsages::TEXTURE_BINDING);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Render world

/// What the render world needs of [`GuestFrames`], cloned cheaply (the pixels are `Arc`s).
#[derive(Resource, Default)]
struct ComposeInput {
    frame: Option<GuestFrame>,
    render_camera: Option<HostCamera>,
}

fn extract_input(mut input: ResMut<ComposeInput>, frames: Extract<Res<GuestFrames>>) {
    // Skip the clone when nothing changed.
    let same = match (&input.frame, frames.frame()) {
        (Some(a), Some(b)) => a.id == b.id,
        (None, None) => true,
        _ => false,
    };
    if !same {
        input.frame = frames.frame().cloned();
    }
    input.render_camera = frames.render_camera().copied();
}

#[derive(Resource)]
struct ComposeGpu {
    engine: Engine,
    guest: Option<GuestGpu>,
}

fn init_gpu(mut commands: Commands, device: Res<RenderDevice>) {
    commands.insert_resource(ComposeGpu {
        engine: Engine::new(&device),
        guest: None,
    });
}

/// Uploads the guest frame when it is new.
fn prepare_guest(
    input: Res<ComposeInput>,
    gpu: Option<ResMut<ComposeGpu>>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
) {
    let Some(mut gpu) = gpu else { return };
    let Some(frame) = &input.frame else {
        gpu.guest = None;
        return;
    };
    if !gpu.guest.as_ref().is_some_and(|g| g.fits(frame)) {
        gpu.guest = Some(GuestGpu::new(
            &device,
            (frame.width, frame.height),
            frame.depth.is_some(),
        ));
    }
    if let Some(guest) = gpu.guest.as_mut() {
        if guest.uploaded != Some(frame.id) {
            guest.upload(&queue, frame);
        }
    }
}

/// What a view needs this frame, computed in `prepare_views`.
#[derive(Component)]
struct ViewCompose {
    params: ComposeParams,
    format: TextureFormat,
}

#[derive(Component)]
struct ViewWarp(WarpBuffers);

#[allow(clippy::type_complexity)]
fn prepare_views(
    mut commands: Commands,
    input: Res<ComposeInput>,
    gpu: Option<ResMut<ComposeGpu>>,
    device: Res<RenderDevice>,
    views: Query<(
        Entity,
        &GuestCompose,
        &ExtractedView,
        &ExtractedCamera,
        &ViewTarget,
        Option<&ViewDepthStencilTexture>,
        Option<&Msaa>,
        Option<&ViewWarp>,
    )>,
    others: Query<Entity, (With<ViewCompose>, Without<GuestCompose>)>,
) {
    for entity in &others {
        commands.entity(entity).remove::<(ViewCompose, ViewWarp)>();
    }
    let Some(mut gpu) = gpu else { return };
    let gpu = &mut *gpu;
    for (entity, settings, view, camera, target, depth, msaa, warp) in &views {
        let skip = |commands: &mut Commands| {
            commands.entity(entity).remove::<(ViewCompose, ViewWarp)>();
        };
        let Some(guest) = gpu.guest.as_ref() else {
            skip(&mut commands);
            continue;
        };
        if camera.hdr {
            warn_once!("GuestCompose: an Hdr camera is left alone (see the compose module docs)");
            skip(&mut commands);
            continue;
        }
        if msaa.is_some_and(|m| *m != Msaa::Off) {
            warn_once!("GuestCompose: a camera with MSAA is left alone, set Msaa::Off on it");
            skip(&mut commands);
            continue;
        }
        let format = target.main_texture_format();
        let srgb = match format {
            TextureFormat::Rgba8UnormSrgb | TextureFormat::Bgra8UnormSrgb => true,
            TextureFormat::Rgba8Unorm | TextureFormat::Bgra8Unorm => false,
            other => {
                warn_once!(
                    "GuestCompose: the target format {other:?} is not an 8-bit one, left alone"
                );
                skip(&mut commands);
                continue;
            }
        };
        let Some(depth) = depth else {
            skip(&mut commands);
            continue;
        };
        if !depth
            .texture()
            .usage()
            .contains(TextureUsages::TEXTURE_BINDING)
        {
            warn_once!(
                "GuestCompose: the camera's depth texture is not bindable (is there a Camera3d?)"
            );
            skip(&mut commands);
            continue;
        }
        let present_view = view_matrix(&view.world_from_view);
        let present_proj = view.clip_from_view.to_cols_array();
        let Some(host_depth) = host_depth_terms(&present_proj) else {
            warn_once!(
                "GuestCompose: the camera's projection is not a perspective one, left alone"
            );
            skip(&mut commands);
            continue;
        };
        let dst_size = (view.viewport.z, view.viewport.w);
        let origin = (view.viewport.x, view.viewport.y);
        if dst_size.0 == 0 || dst_size.1 == 0 {
            skip(&mut commands);
            continue;
        }
        let guest_size = guest.size();

        // Which camera the frame was rendered with: the one in the ring, or (no camera_seq, or one the
        // ring forgot, or reprojection off) the one presented now.
        let render_camera = input.render_camera.as_ref().filter(|_| settings.reproject);
        let same_camera =
            render_camera.is_none_or(|c| c.view == present_view && c.proj == present_proj);
        let warp_matrices = if same_camera && guest_size == dst_size {
            None
        } else {
            let (render_view, render_proj) = match render_camera {
                Some(c) => (c.view, c.proj),
                None => (present_view, present_proj),
            };
            let Some(m) = WarpMatrices::from_matrices(
                &render_view,
                &render_proj,
                &present_view,
                &present_proj,
            ) else {
                skip(&mut commands);
                continue;
            };
            Some(m)
        };

        let params = ComposeParams {
            guest_size,
            dst_size,
            origin,
            has_depth: guest.has_depth(),
            srgb,
            host_depth,
            warp: warp_matrices,
        };
        gpu.engine.prepare_format(&device, format);
        if params.warp.is_some() {
            if !warp.is_some_and(|w| w.0.fits(guest_size, dst_size)) {
                commands
                    .entity(entity)
                    .insert(ViewWarp(WarpBuffers::new(&device, guest_size, dst_size)));
            }
        } else {
            commands.entity(entity).remove::<ViewWarp>();
        }
        commands
            .entity(entity)
            .insert(ViewCompose { params, format });
    }
}

#[allow(clippy::type_complexity)]
fn compose_pass(
    view: ViewQuery<(
        &ViewTarget,
        &ViewDepthStencilTexture,
        &ViewCompose,
        Option<&ViewWarp>,
    )>,
    gpu: Option<Res<ComposeGpu>>,
    device: Res<RenderDevice>,
    mut ctx: RenderContext,
) {
    let (target, depth, compose, warp) = view.into_inner();
    let Some(gpu) = gpu else { return };
    let Some(guest) = gpu.guest.as_ref() else {
        return;
    };
    if !gpu.engine.has_format(compose.format) {
        return;
    }
    if compose.params.warp.is_some() {
        let Some(warp) = warp else { return };
        gpu.engine.encode_warp(
            &device,
            ctx.command_encoder(),
            &compose.params,
            guest,
            &warp.0,
        );
    }
    let write = target.post_process_write();
    gpu.engine.encode_composite(
        &device,
        ctx.command_encoder(),
        &compose.params,
        guest,
        warp.map(|w| &w.0),
        write.source,
        &depth.attachment.texture.default_view,
        write.destination,
        compose.format,
    );
}
