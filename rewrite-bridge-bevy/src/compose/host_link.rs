//! The host's half of the passthrough link, wired to [`GuestFrames`]: listens for a guest, sends it
//! the camera of the [`GuestCompose`] camera once per rendered frame, opens the pixel file it announces
//! and keeps the newest frame it can read.
//!
//! This is optional; an app that runs its own `Link` calls [`GuestFrames::push_camera`] and
//! [`GuestFrames::set_frame`] itself. The plugin does not touch spawns, poses, collision or focus.

use bevy::camera::CameraUpdateSystems;
use bevy::log::{info, warn};
use bevy::prelude::*;
use bevy::transform::TransformSystems;
use rewrite_bridge::link::{Event, FrameChannel, FrameMiss, FrameReader, Link, LinkConfig};

use super::{host_camera, GuestCompose, GuestFrame, GuestFrames};

/// Counters of what [`HostLinkPlugin`] did, for a HUD or a log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostLinkStats {
    /// Whether a guest has completed the handshake.
    pub connected: bool,
    pub cameras_sent: u64,
    /// Frames read and handed to [`GuestFrames`].
    pub frames_shown: u64,
    /// Frame announcements that could not be read (torn, replaced, out of range) or were out of date.
    pub frames_missed: u64,
}

/// The link, and the pixel file being read.
/// (A non-send resource: the link's sockets and channels are not `Sync`, so its systems run on the main
/// thread.)
pub struct HostLink {
    pub link: Link,
    reader: Option<FrameReader>,
    /// The newest frame number shown from the current pixel file.
    newest: u64,
    stats: HostLinkStats,
}

impl HostLink {
    pub fn stats(&self) -> HostLinkStats {
        self.stats
    }
}

/// Starts [`Link::host`] with the given settings and runs it with the app: polls it before `Update`,
/// reads the guest's frames, and sends the camera of the [`GuestCompose`] camera after transforms and
/// cameras have been updated, remembering it in [`GuestFrames`] under the `seq` the link gives it.
///
/// Needs [`GuestComposePlugin`](super::GuestComposePlugin). If the link cannot start (the port is in
/// use, say) it logs the error and does nothing.
pub struct HostLinkPlugin {
    pub config: LinkConfig,
}

impl HostLinkPlugin {
    pub fn new(config: LinkConfig) -> Self {
        Self { config }
    }
}

impl Plugin for HostLinkPlugin {
    fn build(&self, app: &mut App) {
        match Link::host(self.config.clone()) {
            Ok(link) => {
                if let Some(addr) = link.control_addr() {
                    info!("passthrough link: waiting for a guest on {addr}");
                }
                app.insert_non_send(HostLink {
                    link,
                    reader: None,
                    newest: 0,
                    stats: HostLinkStats::default(),
                })
                .add_systems(PreUpdate, poll_link)
                .add_systems(
                    PostUpdate,
                    send_camera
                        .after(TransformSystems::Propagate)
                        .after(CameraUpdateSystems),
                );
            }
            Err(e) => warn!("passthrough link: cannot listen: {e}"),
        }
    }
}

fn poll_link(mut host: NonSendMut<HostLink>, mut frames: ResMut<GuestFrames>) {
    let host = &mut *host;
    for event in host.link.poll() {
        match event {
            Event::Connected(peer) => {
                info!(
                    "passthrough link: guest \"{}\" connected (protocol {})",
                    peer.game, peer.protocol
                );
                host.stats.connected = true;
            }
            Event::Lost { reason } => {
                info!("passthrough link: guest gone: {reason}");
                host.stats.connected = false;
                host.reader = None;
                host.newest = 0;
                frames.clear_frame();
            }
            Event::FrameChannel(FrameChannel { path, .. }) => {
                // A new file numbers its frames from 1 again, so the staleness check starts over.
                host.newest = 0;
                host.reader = match FrameReader::open(std::path::Path::new(&path)) {
                    Ok(reader) => Some(reader),
                    Err(e) => {
                        warn!("passthrough link: cannot open the pixel file {path}: {e}");
                        None
                    }
                };
            }
            Event::Frame { frame: ready, .. } => {
                let Some(reader) = &host.reader else { continue };
                if ready.frame <= host.newest {
                    host.stats.frames_missed += 1;
                    continue;
                }
                let (mut rgba, mut depth) = (Vec::new(), Vec::new());
                match reader.read_depth(&ready, &mut rgba, &mut depth) {
                    Ok(data) => {
                        host.newest = data.frame;
                        let depth = reader.has_depth().then_some(depth);
                        if let Some(frame) = GuestFrame::new(
                            data.frame,
                            data.time_us,
                            data.camera_seq,
                            (data.width, data.height),
                            rgba,
                            depth,
                        ) {
                            frames.set_frame(frame);
                            host.stats.frames_shown += 1;
                        }
                    }
                    Err(FrameMiss::Gone) => {
                        warn!("passthrough link: the pixel file was truncated, dropping it");
                        host.reader = None;
                        host.stats.frames_missed += 1;
                    }
                    Err(_) => host.stats.frames_missed += 1,
                }
            }
            _ => {}
        }
    }
}

fn send_camera(
    mut host: NonSendMut<HostLink>,
    mut frames: ResMut<GuestFrames>,
    cameras: Query<(&GlobalTransform, &Camera), With<GuestCompose>>,
) {
    if !host.link.is_connected() {
        return;
    }
    let Some((transform, camera)) = cameras.iter().next() else {
        return;
    };
    let Some(viewport) = camera.physical_viewport_size() else {
        return;
    };
    match host_camera(transform, camera.clip_from_view(), viewport) {
        Ok(cam) => {
            if let Ok(seq) = host.link.send_camera(&cam) {
                frames.push_camera(seq, cam);
                host.stats.cameras_sent += 1;
            }
        }
        Err(e) => bevy::log::warn_once!("passthrough link: not sending the camera: {e}"),
    }
}
