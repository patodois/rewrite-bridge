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
//!                                       for the one camera with GuestCompose, after the main pass, before
//!                                       tonemapping, FXAA, SMAA and sharpening:
//!                                         [reproject: 3 compute passes]  warp the frame from the camera
//!                                                                        named by camera_seq to this view
//!                                         [composite: 1 fullscreen pass] guest vs the camera's own depth
//! ```
//!
//! # What the host's camera has to be
//!
//! - **A perspective camera whose projection is a plain `clip_from_view`** (Bevy's default reversed,
//!   infinite or finite). An orthographic camera, and a perspective one with an oblique near plane
//!   (`near_clip_plane`, whose depth row varies across the picture), are left alone with a warning:
//!   the host depth buffer cannot be turned into the linear metres the contract compares by one
//!   formula (the contract says an orthographic host does that with its own matrix).
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
//! - **One composited camera.** See [`GuestCompose`]: with several, the one with the lowest `order`.
//!
//! [`GuestCompose`] adds the one thing Bevy needs to make the depth buffer readable
//! (`TextureUsages::TEXTURE_BINDING`) to the camera's `Camera3d` by itself.
//!
//! # Where in the frame, and what it does to the guest
//!
//! The pass runs in `Core3dSystems::EarlyPostProcess`: after the main pass, before tonemapping (a no-op
//! for the low-dynamic-range cameras this accepts), and so before FXAA, SMAA and contrast-adaptive
//! sharpening, which run after tonemapping. That treats the guest as scene content, like the host's own
//! geometry: its edges are anti-aliased against the host's and sharpening sees both. The contract does
//! not say either way (it composites into "the host's own image"); the other choice would leave the
//! guest's hard edges aliased and its bytes untouched. The reference's byte arithmetic is exact for what
//! this pass writes; those later passes are the host's own image processing on top.
//!
//! # Numbers and limits worth knowing
//!
//! - **No maximum age.** A guest frame stays until the next one replaces it or the guest is lost
//!   ([`GuestFrames::frame`]); the contract names no maximum age while the link is up, only how to
//!   measure it ([`GuestFrame::age`]).
//! - **Camera ring.** One host camera goes out per rendered frame (contract section 9.3), and a late frame
//!   needs the camera it was rendered with for at least the last half second (section 9.7), so the ring is
//!   counted in frames: [`DEFAULT_RING_CAPACITY`] is 256, half a second at 500 Hz (32 cover 133 ms at 240 Hz).
//! - **Viewport sides up to 65535 pixels.** The reprojection packs a footprint's bounds as two 16-bit
//!   numbers per axis; a larger viewport is skipped (the guest's frames are at most 16384 a side by the
//!   contract). Frames and viewports that exceed the device's limits (texture size, storage buffer size,
//!   workgroups) are skipped with a warning rather than resized, which would invent pixels; a device
//!   without compute shaders or storage buffers turns compositing off with a warning at start.
//! - **Infinite far plane.** [`host_camera`] reads a projection whose depth row has `|a| < 1e-12` (the
//!   `a` of `z_clip = a * z_view + b`) as an infinite far plane, `near = b`: Bevy's default builds `a`
//!   as exactly 0.
//! - **Stand-in depth.** A frame without a depth plane that has to be resampled (its size differs from the
//!   view's) is warped as if every pixel were 1.0 m away, in the shaders and in the reference's terms
//!   (`depth = 1`); the number only has to be finite and positive because the render camera is the
//!   present camera, where depth cancels, and the depth test is off for such a frame.
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
//!   `rewrite_bridge::link::compose::reproject`), with the stand-in depth above.
//! - **Several composited cameras at once**, since the link carries one host camera.

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
use bevy::log::warn_once;
use bevy::math::{DMat4, Mat4};
use bevy::prelude::*;
use bevy::render::camera::ExtractedCamera;
use bevy::render::extract_component::{ExtractComponent, ExtractComponentPlugin};
use bevy::render::render_resource::{DownlevelFlags, TextureFormat, TextureUsages};
use bevy::render::renderer::{RenderAdapter, RenderContext, RenderDevice, RenderQueue, ViewQuery};
use bevy::render::sync_world::MainEntity;
use bevy::render::view::{ExtractedView, Msaa, ViewDepthStencilTexture, ViewTarget};
use bevy::render::{Extract, ExtractSchedule, Render, RenderApp, RenderStartup, RenderSystems};
use rewrite_bridge::link::compose::CameraRing;
use rewrite_bridge::link::{now_us, Camera as HostCamera, DepthRange};

pub use gpu::{
    check_device, check_sizes, host_depth_terms, ComposeParams, Engine, GuestGpu, WarpBuffers,
    WarpMatrices, MAX_VIEWPORT_SIDE,
};
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

/// How many host cameras [`GuestFrames`] remembers by default: half a second (the least the contract
/// asks for, section 9.7) at 500 Hz. The host sends one camera per rendered frame (section 9.3), so the
/// ring has to be counted in frames, and 32 of them (the contract's figure for 60 Hz) cover only 222 ms
/// at 144 Hz and 133 ms at 240 Hz. A camera is about 170 bytes; a lookup scans the ring once per frame.
pub const DEFAULT_RING_CAPACITY: usize = 256;

/// The main-world side: the newest guest frame and the ring of cameras the host sent, so a frame's
/// `camera_seq` can be turned back into the camera it was rendered with.
#[derive(Resource, Debug)]
pub struct GuestFrames {
    cameras: CameraRing,
    frame: Option<GuestFrame>,
    next_id: u64,
    camera_entity: Option<Entity>,
}

impl Default for GuestFrames {
    fn default() -> Self {
        Self::new(DEFAULT_RING_CAPACITY)
    }
}

impl GuestFrames {
    /// Keeps the last `ring_capacity` cameras. The contract wants at least the last half second, and the
    /// host sends one per frame, so use at least half the refresh rate in frames
    /// ([`DEFAULT_RING_CAPACITY`] covers 500 Hz).
    pub fn new(ring_capacity: usize) -> Self {
        Self {
            cameras: CameraRing::new(ring_capacity),
            frame: None,
            next_id: 1,
            camera_entity: None,
        }
    }

    /// Remembers the camera that went out in the datagram numbered `seq`. Call it with what
    /// `Link::send_camera` returned (the [`HostLinkPlugin`] does).
    pub fn push_camera(&mut self, seq: u32, camera: HostCamera) {
        self.cameras.push(seq, camera);
    }

    /// Makes `frame` the one to show, if it is newer than the current one *within its pixel file*; the
    /// caller decides that (frame numbers restart in a new file). Returns the stored frame's id.
    pub fn set_frame(&mut self, frame: GuestFrame) -> u64 {
        self.replace_frame(frame).0
    }

    /// [`set_frame`](Self::set_frame), also handing back the frame it replaced (so its buffers can be reused).
    pub fn replace_frame(&mut self, mut frame: GuestFrame) -> (u64, Option<GuestFrame>) {
        frame.id = self.next_id;
        self.next_id += 1;
        let id = frame.id;
        (id, self.frame.replace(frame))
    }

    /// Stops showing a guest (it left, or its pixel file went away).
    pub fn clear_frame(&mut self) {
        self.frame = None;
    }

    /// The frame being shown. It stays until another replaces it or the guest is lost: the contract names
    /// no maximum age, so a guest that stops sending frames while the link is up leaves its last one on
    /// screen (compare [`GuestFrame::age`] if you want to drop old ones with [`clear_frame`](Self::clear_frame)).
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

    /// The one camera with [`GuestCompose`] that composites and whose view goes out to the guest (see
    /// [`GuestCompose`] for how it is chosen when there are several). `None` until a frame has run.
    pub fn compose_camera(&self) -> Option<Entity> {
        self.camera_entity
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
/// and the viewport in pixels. `far` is `+inf` for the infinite projection Bevy uses by default (a depth
/// row whose `a` is below 1e-12 in magnitude counts as infinite; Bevy's has exactly 0).
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
///
/// **One camera at a time.** The link carries a single host camera, so the guest renders for one view and
/// a frame can be reprojected for that view only. With several `GuestCompose` cameras the plugin picks
/// the active one with the lowest [`Camera::order`](bevy::camera::Camera::order) (the lowest entity on a
/// tie), sends that one's view, composites into it, and leaves the others alone, with a warning once.
/// The choice is [`GuestFrames::compose_camera`].
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
    /// How many host cameras to remember; see [`GuestFrames::new`] (default [`DEFAULT_RING_CAPACITY`]).
    pub ring_capacity: usize,
}

impl Default for GuestComposePlugin {
    fn default() -> Self {
        Self {
            ring_capacity: DEFAULT_RING_CAPACITY,
        }
    }
}

impl Plugin for GuestComposePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(GuestFrames::new(self.ring_capacity))
            .add_plugins(ExtractComponentPlugin::<GuestCompose>::default())
            .add_systems(
                PostUpdate,
                (make_depth_readable, choose_compose_camera).in_set(ComposeCameraSystems),
            );
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
            // Before tonemapping, FXAA, SMAA and sharpening (all of which are in `PostProcess`, after
            // tonemapping), so the guest is scene content for them: see the module docs.
            .add_systems(Core3d, compose_pass.in_set(Core3dSystems::EarlyPostProcess));
    }
}

/// The systems that decide which camera composites ([`GuestFrames::compose_camera`]); anything that
/// sends the camera to the guest runs after them (the [`HostLinkPlugin`] does).
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ComposeCameraSystems;

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

/// Which of several `(entity, order)` candidates composites: the lowest `order`, the lowest entity index on
/// a tie (the spawned-first one, in an app that does not recycle entities).
pub fn pick_compose_camera(
    candidates: impl IntoIterator<Item = (Entity, isize)>,
) -> Option<Entity> {
    candidates
        .into_iter()
        .min_by_key(|&(entity, order)| (order, entity.index_u32(), entity.generation().to_bits()))
        .map(|(entity, _)| entity)
}

fn choose_compose_camera(
    mut frames: ResMut<GuestFrames>,
    cameras: Query<(Entity, &Camera), With<GuestCompose>>,
) {
    let active = || cameras.iter().filter(|(_, c)| c.is_active);
    let chosen = pick_compose_camera(active().map(|(e, c)| (e, c.order)));
    if active().count() > 1 {
        warn_once!(
            "GuestCompose is on several cameras; only the one with the lowest order composites and \
             is sent to the guest (the link carries one host camera)"
        );
    }
    if frames.camera_entity != chosen {
        frames.camera_entity = chosen;
    }
}

// ---------------------------------------------------------------------------------------------
// Render world

/// What the render world needs of [`GuestFrames`], cloned cheaply (the pixels are `Arc`s).
#[derive(Resource, Default)]
struct ComposeInput {
    frame: Option<GuestFrame>,
    render_camera: Option<HostCamera>,
    /// The main-world entity of the camera that composites
    camera: Option<Entity>,
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
    input.camera = frames.compose_camera();
}

#[derive(Resource)]
struct ComposeGpu {
    engine: Engine,
    guest: Option<GuestGpu>,
}

/// Builds the pipelines, unless the device cannot run them: then compositing is off, with a warning, and
/// the app runs on without it.
fn init_gpu(
    mut commands: Commands,
    device: Res<RenderDevice>,
    adapter: Option<Res<RenderAdapter>>,
) {
    let downlevel = adapter.map_or(DownlevelFlags::all(), |a| {
        a.get_downlevel_capabilities().flags
    });
    if let Err(why) = check_device(&device.limits(), downlevel) {
        warn!("GuestCompose is off: {why}");
        return;
    }
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
    // The frame as it is or not at all: shrinking it to fit would invent pixels
    if let Err(why) = check_sizes(&device.limits(), (frame.width, frame.height), (1, 1), false) {
        warn_once!("GuestCompose: a guest frame is skipped: {why}");
        gpu.guest = None;
        return;
    }
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
        &MainEntity,
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
    for (entity, main, settings, view, camera, target, depth, msaa, warp) in &views {
        let skip = |commands: &mut Commands| {
            commands.entity(entity).remove::<(ViewCompose, ViewWarp)>();
        };
        let Some(guest) = gpu.guest.as_ref() else {
            skip(&mut commands);
            continue;
        };
        // One camera composites (see `GuestCompose`): the one whose view the guest rendered for
        if Some(main.id()) != input.camera {
            skip(&mut commands);
            continue;
        }
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
        // A frame without depth cannot be warped (every pixel's position needs its distance), so it is
        // only resampled when its size differs, as if rendered with the current camera.
        let render_camera = input
            .render_camera
            .as_ref()
            .filter(|_| settings.reproject && guest.has_depth());
        let same_camera =
            render_camera.is_none_or(|c| c.view == present_view && c.proj == present_proj);
        let needs_warp = !(same_camera && guest_size == dst_size);

        // What the device can hold, for this frame in this viewport (shrinking a frame to fit would
        // invent pixels, so a frame that does not fit is skipped)
        if let Err(why) = check_sizes(&device.limits(), guest_size, dst_size, needs_warp) {
            warn_once!("GuestCompose: a view is skipped: {why}");
            skip(&mut commands);
            continue;
        }

        let warp_matrices = if needs_warp {
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
        } else {
            None
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
