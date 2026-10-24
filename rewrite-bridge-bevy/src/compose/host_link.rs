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

use super::{host_camera, ComposeCameraSystems, GuestFrame, GuestFrames};
use std::collections::VecDeque;
use std::sync::Arc;

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

/// The pixel buffers of a frame that was replaced.
type Retired = (Arc<Vec<u8>>, Option<Arc<Vec<f32>>>);

/// The link, and the pixel file being read.
/// (A non-send resource: the link's sockets and channels are not `Sync`, so its systems run on the main
/// thread.)
pub struct HostLink {
    pub link: Link,
    reader: Option<FrameReader>,
    /// The newest frame number shown from the current pixel file.
    newest: u64,
    stats: HostLinkStats,
    /// Pixel buffers of frames that were replaced, waiting until nothing else holds them (the render world
    /// keeps a clone for a frame or two), then read into again instead of allocating two new vectors
    /// of megabytes every frame.
    retired: VecDeque<Retired>,
}

impl HostLink {
    pub fn stats(&self) -> HostLinkStats {
        self.stats
    }
}

/// Starts [`Link::host`] with the given settings and runs it with the app: polls it before `Update`,
/// reads the guest's frames, and sends the camera of the [`GuestCompose`](super::GuestCompose) camera after transforms and
/// cameras have been updated, remembering it in [`GuestFrames`] under the `seq` the link gives it.
///
/// Without [`GuestComposePlugin`](super::GuestComposePlugin) it still runs: it adds [`GuestFrames`] and the
/// choice of the camera to send (the same one the compose plugin registers, once whichever plugin comes
/// first), sends that camera and keeps the newest frame in [`GuestFrames`] for you to use; nothing is
/// drawn then. If the link cannot
/// start (the port is in use, say) it logs the error and does nothing.
///
/// A frame stays on screen until the next one replaces it or the guest is lost; there is no maximum age
/// (see [`GuestFrames::frame`]). Frames of a pixel file are shown only in increasing order, and the count
/// starts again when a new `frame_channel` replaces the file, as the contract says. A pixel file that
/// cannot be opened (a path that is not an absolute regular file, a header that does not check out) and a
/// `Frame` naming a slot or size the file does not have are skipped and counted in
/// [`HostLinkStats::frames_missed`]; the next good one is shown.
///
/// With several [`GuestCompose`](super::GuestCompose) cameras it sends the one that composites
/// ([`GuestFrames::compose_camera`]).
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
                super::register_camera_choice(app);
                app.insert_non_send(HostLink {
                    link,
                    reader: None,
                    newest: 0,
                    stats: HostLinkStats::default(),
                    retired: VecDeque::new(),
                })
                .add_systems(PreUpdate, poll_link)
                .add_systems(
                    PostUpdate,
                    send_camera
                        .after(TransformSystems::Propagate)
                        .after(CameraUpdateSystems)
                        .after(ComposeCameraSystems),
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
                // Reuse the buffers of a frame nothing holds any more
                let (mut rgba, mut depth) = (Vec::new(), Vec::new());
                if host.retired.front().is_some_and(|(c, d)| {
                    Arc::strong_count(c) == 1
                        && d.as_ref().is_none_or(|d| Arc::strong_count(d) == 1)
                }) {
                    let (c, d) = host.retired.pop_front().unwrap();
                    rgba = Arc::try_unwrap(c).unwrap_or_default();
                    depth = d.and_then(|d| Arc::try_unwrap(d).ok()).unwrap_or_default();
                }
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
                            let (_, old) = frames.replace_frame(frame);
                            if let Some(old) = old {
                                host.retired.push_back((old.rgba, old.depth));
                                // Frames are replaced faster than the render world lets go of them
                                // only if it is stuck; do not hoard
                                while host.retired.len() > 4 {
                                    host.retired.pop_front();
                                }
                            }
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

pub(super) fn send_camera(
    mut host: NonSendMut<HostLink>,
    mut frames: ResMut<GuestFrames>,
    cameras: Query<(&GlobalTransform, &Camera)>,
) {
    if !host.link.is_connected() {
        return;
    }
    // The camera that composites, one for the whole link
    let Some((transform, camera)) = frames
        .compose_camera()
        .and_then(|entity| cameras.get(entity).ok())
    else {
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

#[cfg(test)]
mod tests {
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use rewrite_bridge::link::{FrameFileOptions, FrameReady, FrameWriter};

    use super::*;

    /// A host app with only the link plugin: no render app, no `GuestComposePlugin`.
    fn host_app() -> (App, u16) {
        let mut app = App::new();
        app.add_plugins(HostLinkPlugin::new(LinkConfig {
            control_port: 0,
            ..LinkConfig::new("test host")
        }));
        let port = app
            .world()
            .non_send::<HostLink>()
            .link
            .control_addr()
            .expect("listening")
            .port();
        (app, port)
    }

    fn guest_link(port: u16) -> Link {
        Link::guest(LinkConfig {
            control_port: port,
            reconnect: Duration::from_millis(50),
            ..LinkConfig::new("test guest")
        })
        .expect("a guest link")
    }

    fn stats(app: &App) -> HostLinkStats {
        app.world().non_send::<HostLink>().stats()
    }

    fn shown(app: &App) -> Option<(u64, bool, u8)> {
        let frames = app.world().resource::<GuestFrames>();
        frames
            .frame()
            .map(|f| (f.frame, f.depth.is_some(), f.rgba[0]))
    }

    /// Runs the host and the guests until `done`, at most 10 s.
    fn pump(app: &mut App, guests: &mut [&mut Link], done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(app) {
            assert!(Instant::now() < deadline, "timed out: {:?}", stats(app));
            app.update();
            for g in guests.iter_mut() {
                g.poll();
            }
            sleep(Duration::from_millis(2));
        }
    }

    /// Lets everything in flight arrive.
    fn settle(app: &mut App, guests: &mut [&mut Link]) {
        for _ in 0..40 {
            app.update();
            for g in guests.iter_mut() {
                g.poll();
            }
            sleep(Duration::from_millis(2));
        }
    }

    fn writer(tag: &str) -> FrameWriter {
        FrameWriter::create_temp_with(tag, &FrameFileOptions::new(8, 8, 3).with_depth())
            .expect("a pixel file")
    }

    fn write(w: &mut FrameWriter, fill: u8) -> FrameReady {
        w.write_depth(&[fill; 8 * 8 * 4], Some(&[2.0; 64]), 8, 8, 0, now_us())
            .expect("a frame")
    }

    use rewrite_bridge::link::now_us;

    #[test]
    fn the_link_survives_bad_channels_bad_slots_a_bye_and_a_new_pixel_file() {
        let (mut app, port) = host_app();
        // no GuestComposePlugin: the plugin made the resource itself
        assert!(app.world().get_resource::<GuestFrames>().is_some());
        let mut guest = guest_link(port);
        pump(&mut app, &mut [&mut guest], |a| stats(a).connected);

        // A channel whose path is not an absolute regular file is refused, and frames announced for it
        // are ignored
        for path in [
            "relative.frames",
            "/rewrite-bridge-no-such-dir/x.frames",
            "/",
        ] {
            guest
                .announce_frames(&rewrite_bridge::link::FrameChannel {
                    path: path.into(),
                    width: 8,
                    height: 8,
                    slots: 3,
                })
                .unwrap();
            let ready = FrameReady {
                frame: 1,
                slot: 0,
                camera_seq: 0,
                width: 8,
                height: 8,
            };
            guest.send_frame(&ready).unwrap();
            settle(&mut app, &mut [&mut guest]);
            assert_eq!(shown(&app), None, "{path}");
        }
        assert_eq!(stats(&app).frames_shown, 0);

        // A good channel and a good frame
        let mut w1 = writer("hl-one");
        guest.announce_frames(w1.channel()).unwrap();
        let ready = write(&mut w1, 10);
        assert_eq!(ready.frame, 1);
        guest.send_frame(&ready).unwrap();
        pump(&mut app, &mut [&mut guest], |a| stats(a).frames_shown == 1);
        assert_eq!(shown(&app), Some((1, true, 10)));

        // Announcements the file cannot honour, numbered as the writer numbers its frames: a slot the file
        // does not have, a size beyond the file's, a slot that does not hold that frame. Each is a miss
        // and none of them shows anything
        let missed = stats(&app).frames_missed;
        let r = write(&mut w1, 2);
        guest.send_frame(&FrameReady { slot: 7, ..r }).unwrap();
        let r = write(&mut w1, 3);
        guest.send_frame(&FrameReady { width: 9, ..r }).unwrap();
        let r = write(&mut w1, 4);
        guest
            .send_frame(&FrameReady {
                slot: (r.slot + 1) % 3,
                ..r
            })
            .unwrap();
        pump(&mut app, &mut [&mut guest], |a| {
            stats(a).frames_missed == missed + 3
        });
        assert_eq!(
            shown(&app),
            Some((1, true, 10)),
            "a bad frame leaves the picture alone"
        );

        // and the next good one is shown
        let ready = write(&mut w1, 20);
        assert_eq!(ready.frame, 5);
        guest.send_frame(&ready).unwrap();
        pump(&mut app, &mut [&mut guest], |a| stats(a).frames_shown == 2);
        assert_eq!(shown(&app), Some((5, true, 20)));

        // A new pixel file in the same session numbers its frames from 1 again, and they are shown
        let mut w2 = writer("hl-two");
        guest.announce_frames(w2.channel()).unwrap();
        let ready = write(&mut w2, 30);
        assert_eq!(ready.frame, 1);
        guest.send_frame(&ready).unwrap();
        pump(&mut app, &mut [&mut guest], |a| stats(a).frames_shown == 3);
        assert_eq!(shown(&app), Some((1, true, 30)));
        drop(w1);

        // The guest says bye: the picture goes, and the numbering starts over
        guest.disconnect("bye");
        pump(&mut app, &mut [&mut guest], |a| !stats(a).connected);
        assert_eq!(
            shown(&app),
            None,
            "the frame is cleared when the guest is lost"
        );
        assert_eq!(app.world().non_send::<HostLink>().newest, 0);
        drop((guest, w2));

        // A new guest, a new pixel file, numbering from 1: shown
        let mut guest = guest_link(port);
        pump(&mut app, &mut [&mut guest], |a| stats(a).connected);
        let mut w3 = writer("hl-three");
        guest.announce_frames(w3.channel()).unwrap();
        let ready = write(&mut w3, 40);
        assert_eq!(ready.frame, 1);
        guest.send_frame(&ready).unwrap();
        pump(&mut app, &mut [&mut guest], |a| stats(a).frames_shown == 4);
        assert_eq!(shown(&app), Some((1, true, 40)));
    }

    #[test]
    fn frame_buffers_are_reused_instead_of_allocated_every_frame() {
        let (mut app, port) = host_app();
        let mut guest = guest_link(port);
        pump(&mut app, &mut [&mut guest], |a| stats(a).connected);
        let mut w = writer("hl-reuse");
        guest.announce_frames(w.channel()).unwrap();
        let mut seen = std::collections::HashSet::new();
        for i in 1..=12u64 {
            let ready = write(&mut w, i as u8);
            guest.send_frame(&ready).unwrap();
            pump(&mut app, &mut [&mut guest], |a| stats(a).frames_shown == i);
            let frames = app.world().resource::<GuestFrames>();
            seen.insert(frames.frame().unwrap().rgba.as_ptr() as usize);
        }
        assert!(
            seen.len() <= 3,
            "{} different buffers for 12 frames",
            seen.len()
        );
    }

    #[test]
    fn alone_the_plugin_sends_the_camera_that_composites_and_remembers_it() {
        use bevy::camera::{CameraProjection, PerspectiveProjection, RenderTargetInfo, Viewport};
        use rewrite_bridge::link::Camera as HostCamera;

        use super::super::{view_matrix, GuestCompose};

        // No GuestComposePlugin, no render app: the plugin has to pick the camera itself
        let (mut app, port) = host_app();
        let proj = PerspectiveProjection::default().get_clip_from_view();
        let camera = |order: isize, viewport: UVec2, x: f32| {
            let mut c = Camera {
                order,
                viewport: Some(Viewport {
                    physical_size: viewport,
                    ..default()
                }),
                ..default()
            };
            c.computed.target_info = Some(RenderTargetInfo {
                physical_size: UVec2::new(320, 180),
                scale_factor: 1.0,
            });
            c.computed.clip_from_view = proj;
            (
                c,
                GlobalTransform::from(Transform::from_xyz(x, 1.0, 2.0)),
                GuestCompose::default(),
            )
        };
        app.world_mut().spawn(camera(0, UVec2::new(320, 180), 0.0));
        let chosen = app
            .world_mut()
            .spawn(camera(-1, UVec2::new(160, 90), 5.0))
            .id();

        let mut guest = guest_link(port);
        let mut got: Vec<(u32, HostCamera)> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while got.len() < 5 {
            assert!(Instant::now() < deadline, "no cameras: {:?}", stats(&app));
            app.update();
            for event in guest.poll() {
                if let Event::Camera { header, camera } = event {
                    got.push((header.seq, camera));
                }
            }
            sleep(Duration::from_millis(2));
        }
        let frames = app.world().resource::<GuestFrames>();
        assert_eq!(frames.compose_camera(), Some(chosen));
        let want_view = view_matrix(&GlobalTransform::from(Transform::from_xyz(5.0, 1.0, 2.0)));
        for (seq, camera) in &got {
            // the lowest order's camera: its viewport and its place
            assert_eq!(camera.viewport, [160, 90]);
            assert_eq!(camera.view, want_view);
            // and remembered under the seq it went out with
            assert_eq!(frames.cameras().get(*seq), Some(camera));
        }
        assert!(stats(&app).cameras_sent >= 5);
    }
}
