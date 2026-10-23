//! Passthrough link: exchange camera, poses, collision and frames with another process on
//! 127.0.0.1, so a rewrite can act as the host or the guest of a two-game "passthrough" mod.
//!
//! `docs/CONTRACT.md` defines the protocol; this module is one implementation of it. It has three
//! layers, usable separately:
//!
//! - the wire codec: [`encode_datagram`] / [`decode_datagram`] for the binary UDP datagrams and
//!   [`Control::to_line`] / [`Control::parse`] for the JSON-lines control channel;
//! - [`Link`], which owns the sockets, does the handshake, heartbeat and watchdog, and is driven by
//!   a non-blocking [`Link::poll`] from a game loop or a Bevy system;
//! - [`FrameWriter`] / [`FrameReader`] for RGBA frames in a memory-mapped file.
//!
//! The link only exchanges what each process chooses to publish. It never touches another process.

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{fence, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use memmap2::{Mmap, MmapMut};
use serde_json::{json, Map, Value};

/// The protocol version this implementation speaks.
pub const PROTOCOL_VERSION: u16 = 1;
/// The oldest protocol version this implementation accepts.
pub const MIN_PROTOCOL_VERSION: u16 = 1;
/// First four bytes of every datagram: "RBLK" read as a little-endian u32.
pub const MAGIC: u32 = 0x4B4C_4252;
/// Default TCP control port the host listens on.
pub const DEFAULT_CONTROL_PORT: u16 = 47800;

pub const HEADER_LEN: usize = 24;
pub const MAX_DATAGRAM: usize = 8192;
pub const CAMERA_LEN: usize = 152;
pub const POSE_HEADER_LEN: usize = 8;
pub const POSE_LEN: usize = 80;
pub const MAX_POSES: usize = (MAX_DATAGRAM - HEADER_LEN - POSE_HEADER_LEN) / POSE_LEN;
pub const COLLISION_HEADER_LEN: usize = 32;
pub const TRIANGLE_LEN: usize = 36;
pub const MAX_TRIANGLES: usize = (MAX_DATAGRAM - HEADER_LEN - COLLISION_HEADER_LEN) / TRIANGLE_LEN;
pub const FRAME_LEN: usize = 24;
/// Longest control line, newline included.
pub const MAX_LINE: usize = 65536;
/// Unsent control bytes beyond which the peer counts as lost.
pub const MAX_PENDING_CONTROL: usize = 1 << 20;

/// [`Pose::flags`] bit: do not interpolate from the entity's previous pose.
pub const POSE_TELEPORT: u32 = 1;

pub const KIND_HEARTBEAT: u8 = 1;
pub const KIND_CAMERA: u8 = 2;
pub const KIND_POSE: u8 = 3;
pub const KIND_COLLISION: u8 = 4;
pub const KIND_FRAME: u8 = 5;

/// Microseconds since the Unix epoch on the wall clock, the timestamp every message carries.
pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

/// Serial-number comparison (RFC 1982) for wrapping 32-bit counters: is `a` newer than `b`?
pub fn seq_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// The session version two sides agree on, or `None` when their ranges do not overlap.
/// Ranges are `(min_protocol, protocol)`.
pub fn negotiate(host: (u16, u16), guest: (u16, u16)) -> Option<u16> {
    let v = host.1.min(guest.1);
    (v >= host.0.max(guest.0)).then_some(v)
}

// ---------------------------------------------------------------------------------------------
// Errors

/// A message that cannot be encoded or decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    /// Fewer bytes than the header or the payload needs.
    Truncated {
        need: usize,
        got: usize,
    },
    BadMagic(u32),
    BadVersion(u16),
    /// A datagram whose length does not match its kind and counts.
    BadLength {
        kind: u8,
        expected: usize,
        got: usize,
    },
    /// A field outside its allowed values.
    BadValue(&'static str),
    /// A control line that is not valid JSON of the expected shape.
    BadControl(String),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { need, got } => write!(f, "truncated: need {need} bytes, got {got}"),
            Self::BadMagic(m) => write!(f, "bad magic {m:#010x}"),
            Self::BadVersion(v) => write!(f, "unsupported version {v}"),
            Self::BadLength {
                kind,
                expected,
                got,
            } => write!(f, "kind {kind}: expected {expected} bytes, got {got}"),
            Self::BadValue(what) => write!(f, "bad value: {what}"),
            Self::BadControl(why) => write!(f, "bad control line: {why}"),
        }
    }
}

impl Error for WireError {}

/// Errors from [`Link`]'s send methods.
#[derive(Debug)]
pub enum LinkError {
    /// No peer has completed the handshake.
    NotConnected,
    Wire(WireError),
    Io(io::Error),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConnected => f.write_str("no peer connected"),
            Self::Wire(e) => e.fmt(f),
            Self::Io(e) => e.fmt(f),
        }
    }
}

impl Error for LinkError {}

impl From<WireError> for LinkError {
    fn from(e: WireError) -> Self {
        Self::Wire(e)
    }
}

impl From<io::Error> for LinkError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------------------------
// Message types

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Host,
    Guest,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Guest => "guest",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "host" => Some(Self::Host),
            "guest" => Some(Self::Guest),
            _ => None,
        }
    }
}

/// The datagram header, minus `magic` and `kind` (the kind is the [`Datagram`] variant).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub version: u16,
    pub flags: u8,
    pub session: u32,
    pub seq: u32,
    pub time_us: u64,
}

/// Clip-space depth convention of [`Camera::proj`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepthRange {
    /// OpenGL: near maps to -1, far to +1.
    NegOneToOne = 0,
    /// Direct3D, Vulkan, Metal: near 0, far 1.
    ZeroToOne = 1,
    /// Reversed: near 1, far 0.
    Reversed = 2,
}

/// The host's view, sent once per rendered frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    /// World to view, column-major. The camera looks down its local -Z.
    pub view: [f32; 16],
    /// View to clip, column-major.
    pub proj: [f32; 16],
    /// Vertical field of view, radians.
    pub fov_y: f32,
    pub near: f32,
    /// `f32::INFINITY` for an infinite projection.
    pub far: f32,
    /// Width and height in pixels.
    pub viewport: [u32; 2],
    pub depth_range: DepthRange,
}

impl Camera {
    /// A right-handed look-at camera with an OpenGL-style perspective projection (`far` may be
    /// infinite).
    pub fn look_at(
        eye: [f32; 3],
        target: [f32; 3],
        fov_y: f32,
        near: f32,
        far: f32,
        viewport: [u32; 2],
    ) -> Self {
        let f = normalize(sub(target, eye));
        let s = normalize(cross(f, [0.0, 1.0, 0.0]));
        let u = cross(s, f);
        #[rustfmt::skip]
        let view = [
            s[0], u[0], -f[0], 0.0,
            s[1], u[1], -f[1], 0.0,
            s[2], u[2], -f[2], 0.0,
            -dot(s, eye), -dot(u, eye), dot(f, eye), 1.0,
        ];
        let aspect = viewport[0] as f32 / viewport[1].max(1) as f32;
        let t = 1.0 / (fov_y * 0.5).tan();
        let mut proj = [0.0; 16];
        proj[0] = t / aspect;
        proj[5] = t;
        proj[11] = -1.0;
        if far.is_finite() {
            proj[10] = (far + near) / (near - far);
            proj[14] = 2.0 * far * near / (near - far);
        } else {
            proj[10] = -1.0;
            proj[14] = -2.0 * near;
        }
        Self {
            view,
            proj,
            fov_y,
            near,
            far,
            viewport,
            depth_range: DepthRange::NegOneToOne,
        }
    }

    /// The camera's world position, assuming `view` is a rigid transform.
    pub fn eye(&self) -> [f32; 3] {
        let v = &self.view;
        let t = [v[12], v[13], v[14]];
        // eye = -R^T t, with R[row][col] = v[col * 4 + row].
        std::array::from_fn(|c| -(0..3).map(|r| v[c * 4 + r] * t[r]).sum::<f32>())
    }

    /// The direction the camera looks in, in world space.
    pub fn forward(&self) -> [f32; 3] {
        let v = &self.view;
        [-v[2], -v[6], -v[10]]
    }
}

/// One entity's pose, in world space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    /// The sender's entity id, as given in its `spawn`.
    pub entity: u32,
    /// [`POSE_TELEPORT`] or 0.
    pub flags: u32,
    /// When the pose was sampled, [`now_us`] clock.
    pub time_us: u64,
    pub translation: [f32; 3],
    /// Quaternion x, y, z, w.
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
    /// m/s.
    pub velocity: [f32; 3],
    /// Axis times rad/s.
    pub angular_velocity: [f32; 3],
}

impl Pose {
    /// An identity pose at the origin, at rest.
    pub fn new(entity: u32, time_us: u64) -> Self {
        Self {
            entity,
            flags: 0,
            time_us,
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            velocity: [0.0; 3],
            angular_velocity: [0.0; 3],
        }
    }
}

/// Vertices a, b, c of a triangle, counter-clockwise from its front.
pub type Triangle = [[f32; 3]; 3];

/// One datagram's share of a collision snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct CollisionChunk {
    pub query: u32,
    pub chunk_index: u16,
    pub chunk_count: u16,
    pub center: [f32; 3],
    pub radius: f32,
    pub triangles: Vec<Triangle>,
}

/// A whole collision snapshot: the sender's triangles within `radius` of `center`.
#[derive(Clone, Debug, PartialEq)]
pub struct Collision {
    pub query: u32,
    pub center: [f32; 3],
    pub radius: f32,
    pub triangles: Vec<Triangle>,
}

impl Collision {
    /// Splits the snapshot into datagram-sized chunks; an empty one is a single empty chunk.
    pub fn chunks(&self) -> Result<Vec<CollisionChunk>, WireError> {
        let n = self.triangles.len().div_ceil(MAX_TRIANGLES).max(1);
        let count = u16::try_from(n).map_err(|_| WireError::BadValue("too many triangles"))?;
        Ok((0..n)
            .map(|i| CollisionChunk {
                query: self.query,
                chunk_index: i as u16,
                chunk_count: count,
                center: self.center,
                radius: self.radius,
                triangles: self
                    .triangles
                    .iter()
                    .skip(i * MAX_TRIANGLES)
                    .take(MAX_TRIANGLES)
                    .copied()
                    .collect(),
            })
            .collect())
    }
}

/// Announces that a frame was written to a slot of the sender's pixel file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameReady {
    /// Frame number, from 1.
    pub frame: u64,
    pub slot: u32,
    /// Header `seq` of the [`Camera`] the frame was rendered with, 0 if none.
    pub camera_seq: u32,
    pub width: u32,
    pub height: u32,
}

/// The payload of a datagram, one variant per `kind`.
#[derive(Clone, Debug, PartialEq)]
pub enum Datagram {
    Heartbeat,
    Camera(Camera),
    /// 1 to [`MAX_POSES`] poses.
    Pose(Vec<Pose>),
    Collision(CollisionChunk),
    Frame(FrameReady),
    /// A kind this version does not know. Receivers ignore it; it cannot be encoded.
    Unknown {
        kind: u8,
        payload_len: usize,
    },
}

impl Datagram {
    pub fn kind(&self) -> u8 {
        match self {
            Self::Heartbeat => KIND_HEARTBEAT,
            Self::Camera(_) => KIND_CAMERA,
            Self::Pose(_) => KIND_POSE,
            Self::Collision(_) => KIND_COLLISION,
            Self::Frame(_) => KIND_FRAME,
            Self::Unknown { kind, .. } => *kind,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Binary codec

struct Out<'a>(&'a mut Vec<u8>);

impl Out<'_> {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32s(&mut self, v: &[f32]) {
        for x in v {
            self.0.extend_from_slice(&x.to_le_bytes());
        }
    }
}

/// Reads little-endian fields; every read is bounds-checked, so garbage cannot panic.
struct In<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> In<'a> {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let s = self
            .b
            .get(self.at..self.at + N)
            .ok_or(WireError::Truncated {
                need: self.at + N,
                got: self.b.len(),
            })?;
        self.at += N;
        Ok(s.try_into().expect("slice of length N"))
    }
    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.bytes::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16, WireError> {
        self.bytes().map(u16::from_le_bytes)
    }
    fn u32(&mut self) -> Result<u32, WireError> {
        self.bytes().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Result<u64, WireError> {
        self.bytes().map(u64::from_le_bytes)
    }
    fn f32(&mut self) -> Result<f32, WireError> {
        self.bytes().map(f32::from_le_bytes)
    }
    fn f32s<const N: usize>(&mut self) -> Result<[f32; N], WireError> {
        let mut v = [0.0; N];
        for x in &mut v {
            *x = self.f32()?;
        }
        Ok(v)
    }
}

/// Writes `header` and `body` as one datagram into `out`, replacing its contents.
pub fn encode_datagram(
    header: &Header,
    body: &Datagram,
    out: &mut Vec<u8>,
) -> Result<(), WireError> {
    out.clear();
    let mut w = Out(out);
    w.u32(MAGIC);
    w.u16(header.version);
    w.u8(body.kind());
    w.u8(header.flags);
    w.u32(header.session);
    w.u32(header.seq);
    w.u64(header.time_us);
    match body {
        Datagram::Heartbeat => {}
        Datagram::Camera(c) => {
            w.f32s(&c.view);
            w.f32s(&c.proj);
            w.f32s(&[c.fov_y, c.near, c.far]);
            w.u32(c.viewport[0]);
            w.u32(c.viewport[1]);
            w.u32(c.depth_range as u32);
        }
        Datagram::Pose(poses) => {
            if poses.is_empty() || poses.len() > MAX_POSES {
                return Err(WireError::BadValue("pose count must be 1 to MAX_POSES"));
            }
            w.u32(poses.len() as u32);
            w.u32(0);
            for p in poses {
                w.u32(p.entity);
                w.u32(p.flags);
                w.u64(p.time_us);
                w.f32s(&p.translation);
                w.f32s(&p.rotation);
                w.f32s(&p.scale);
                w.f32s(&p.velocity);
                w.f32s(&p.angular_velocity);
            }
        }
        Datagram::Collision(c) => {
            if c.chunk_count == 0 || c.chunk_index >= c.chunk_count {
                return Err(WireError::BadValue("chunk_index must be below chunk_count"));
            }
            if c.triangles.len() > MAX_TRIANGLES {
                return Err(WireError::BadValue("more than MAX_TRIANGLES in a chunk"));
            }
            w.u32(c.query);
            w.u16(c.chunk_index);
            w.u16(c.chunk_count);
            w.f32s(&c.center);
            w.f32s(&[c.radius]);
            w.u32(c.triangles.len() as u32);
            w.u32(0);
            for t in &c.triangles {
                for v in t {
                    w.f32s(v);
                }
            }
        }
        Datagram::Frame(f) => {
            if f.frame == 0 {
                return Err(WireError::BadValue("frame numbers start at 1"));
            }
            w.u64(f.frame);
            w.u32(f.slot);
            w.u32(f.camera_seq);
            w.u32(f.width);
            w.u32(f.height);
        }
        Datagram::Unknown { .. } => return Err(WireError::BadValue("unknown kind")),
    }
    debug_assert!(out.len() <= MAX_DATAGRAM);
    Ok(())
}

/// Reads one datagram's header (checking magic and version) without its payload.
pub fn decode_header(buf: &[u8]) -> Result<(Header, u8), WireError> {
    if buf.len() < HEADER_LEN {
        return Err(WireError::Truncated {
            need: HEADER_LEN,
            got: buf.len(),
        });
    }
    let mut r = In { b: buf, at: 0 };
    let magic = r.u32()?;
    if magic != MAGIC {
        return Err(WireError::BadMagic(magic));
    }
    let version = r.u16()?;
    if version != PROTOCOL_VERSION {
        return Err(WireError::BadVersion(version));
    }
    let kind = r.u8()?;
    let header = Header {
        version,
        flags: r.u8()?,
        session: r.u32()?,
        seq: r.u32()?,
        time_us: r.u64()?,
    };
    Ok((header, kind))
}

/// Decodes one datagram. Anything malformed is an error; nothing panics on any input.
pub fn decode_datagram(buf: &[u8]) -> Result<(Header, Datagram), WireError> {
    let (header, kind) = decode_header(buf)?;
    let payload = buf.len() - HEADER_LEN;
    let mut r = In {
        b: buf,
        at: HEADER_LEN,
    };
    let exact = |expected: usize| {
        if payload == expected {
            Ok(())
        } else {
            Err(WireError::BadLength {
                kind,
                expected: HEADER_LEN + expected,
                got: buf.len(),
            })
        }
    };
    let body = match kind {
        KIND_HEARTBEAT => {
            exact(0)?;
            Datagram::Heartbeat
        }
        KIND_CAMERA => {
            exact(CAMERA_LEN)?;
            let view = r.f32s()?;
            let proj = r.f32s()?;
            let [fov_y, near, far] = r.f32s()?;
            let viewport = [r.u32()?, r.u32()?];
            let depth_range = match r.u32()? {
                0 => DepthRange::NegOneToOne,
                1 => DepthRange::ZeroToOne,
                2 => DepthRange::Reversed,
                _ => return Err(WireError::BadValue("depth_range")),
            };
            Datagram::Camera(Camera {
                view,
                proj,
                fov_y,
                near,
                far,
                viewport,
                depth_range,
            })
        }
        KIND_POSE => {
            let count = r.u32()? as usize;
            if count == 0 || count > MAX_POSES {
                return Err(WireError::BadValue("pose count"));
            }
            exact(POSE_HEADER_LEN + POSE_LEN * count)?;
            r.u32()?;
            let mut poses = Vec::with_capacity(count);
            for _ in 0..count {
                poses.push(Pose {
                    entity: r.u32()?,
                    flags: r.u32()?,
                    time_us: r.u64()?,
                    translation: r.f32s()?,
                    rotation: r.f32s()?,
                    scale: r.f32s()?,
                    velocity: r.f32s()?,
                    angular_velocity: r.f32s()?,
                });
            }
            Datagram::Pose(poses)
        }
        KIND_COLLISION => {
            let query = r.u32()?;
            let chunk_index = r.u16()?;
            let chunk_count = r.u16()?;
            let center = r.f32s()?;
            let radius = r.f32()?;
            let n = r.u32()? as usize;
            r.u32()?;
            if chunk_count == 0 || chunk_index >= chunk_count {
                return Err(WireError::BadValue("chunk_index"));
            }
            if n > MAX_TRIANGLES {
                return Err(WireError::BadValue("triangle_count"));
            }
            exact(COLLISION_HEADER_LEN + TRIANGLE_LEN * n)?;
            let mut triangles: Vec<Triangle> = Vec::with_capacity(n);
            for _ in 0..n {
                triangles.push([r.f32s()?, r.f32s()?, r.f32s()?]);
            }
            Datagram::Collision(CollisionChunk {
                query,
                chunk_index,
                chunk_count,
                center,
                radius,
                triangles,
            })
        }
        KIND_FRAME => {
            exact(FRAME_LEN)?;
            let f = FrameReady {
                frame: r.u64()?,
                slot: r.u32()?,
                camera_seq: r.u32()?,
                width: r.u32()?,
                height: r.u32()?,
            };
            if f.frame == 0 {
                return Err(WireError::BadValue("frame 0"));
            }
            Datagram::Frame(f)
        }
        _ => Datagram::Unknown {
            kind,
            payload_len: payload,
        },
    };
    Ok((header, body))
}

// ---------------------------------------------------------------------------------------------
// Control messages

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub role: Role,
    pub game: String,
    /// Highest supported version; in the host's reply, the chosen version.
    pub protocol: u16,
    pub min_protocol: u16,
    pub udp_port: u16,
    pub heartbeat_ms: u32,
    pub timeout_ms: u32,
    /// Set in the host's reply only.
    pub session: Option<u32>,
}

/// An entity of the sender's, drawn with a provider's asset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawn {
    pub entity: u32,
    /// `Provider::game()` of the asset's game.
    pub provider: String,
    /// `AssetInfo::id` of the asset.
    pub asset: String,
    pub name: Option<String>,
}

/// Announces the sender's pixel file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameChannel {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub slots: u32,
}

/// A control-channel message, one JSON line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    Hello(Hello),
    Spawn(Spawn),
    Despawn {
        entity: u32,
    },
    /// From the host: who has input focus now. From the guest: a request.
    Input {
        focus: Role,
        reason: Option<String>,
    },
    FrameChannel(FrameChannel),
    Bye {
        reason: String,
    },
    /// A `type` this version does not know; ignored.
    Unknown(String),
}

impl Control {
    /// The message as one JSON line, newline included.
    pub fn to_line(&self) -> String {
        let mut v = match self {
            Self::Hello(h) => {
                let mut v = json!({
                    "type": "hello",
                    "role": h.role.as_str(),
                    "game": h.game,
                    "protocol": h.protocol,
                    "min_protocol": h.min_protocol,
                    "udp_port": h.udp_port,
                    "heartbeat_ms": h.heartbeat_ms,
                    "timeout_ms": h.timeout_ms,
                });
                if let Some(s) = h.session {
                    v["session"] = s.into();
                }
                v
            }
            Self::Spawn(s) => {
                let mut v = json!({
                    "type": "spawn",
                    "entity": s.entity,
                    "provider": s.provider,
                    "asset": s.asset,
                });
                if let Some(n) = &s.name {
                    v["name"] = n.as_str().into();
                }
                v
            }
            Self::Despawn { entity } => json!({ "type": "despawn", "entity": entity }),
            Self::Input { focus, reason } => {
                let mut v = json!({ "type": "input", "focus": focus.as_str() });
                if let Some(r) = reason {
                    v["reason"] = r.as_str().into();
                }
                v
            }
            Self::FrameChannel(c) => json!({
                "type": "frame_channel",
                "path": c.path,
                "width": c.width,
                "height": c.height,
                "slots": c.slots,
            }),
            Self::Bye { reason } => json!({ "type": "bye", "reason": reason }),
            Self::Unknown(t) => json!({ "type": t }),
        }
        .to_string();
        v.push('\n');
        v
    }

    /// Parses one line (with or without its `\r\n`). Unknown types give [`Control::Unknown`];
    /// a known type with missing or out-of-range fields is an error.
    pub fn parse(line: &str) -> Result<Self, WireError> {
        let bad = |why: &str| WireError::BadControl(why.to_owned());
        let line = line.trim_end_matches(['\n', '\r']);
        if line.len() >= MAX_LINE {
            return Err(bad("line too long"));
        }
        let v: Value =
            serde_json::from_str(line).map_err(|e| WireError::BadControl(e.to_string()))?;
        let o = v.as_object().ok_or_else(|| bad("not an object"))?;
        let ty = o
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("no string \"type\""))?;
        let f = Fields(o);
        Ok(match ty {
            "hello" => Self::Hello(Hello {
                role: Role::parse(f.str("role")?).ok_or_else(|| bad("role"))?,
                game: f.str("game")?.to_owned(),
                protocol: f.int("protocol")?,
                min_protocol: f.int("min_protocol")?,
                udp_port: f.int("udp_port")?,
                heartbeat_ms: f.int("heartbeat_ms")?,
                timeout_ms: f.int("timeout_ms")?,
                session: f.opt_int("session")?,
            }),
            "spawn" => Self::Spawn(Spawn {
                entity: f.int("entity")?,
                provider: f.str("provider")?.to_owned(),
                asset: f.str("asset")?.to_owned(),
                name: f.opt_str("name")?,
            }),
            "despawn" => Self::Despawn {
                entity: f.int("entity")?,
            },
            "input" => Self::Input {
                focus: Role::parse(f.str("focus")?).ok_or_else(|| bad("focus"))?,
                reason: f.opt_str("reason")?,
            },
            "frame_channel" => Self::FrameChannel(FrameChannel {
                path: f.str("path")?.to_owned(),
                width: f.int("width")?,
                height: f.int("height")?,
                slots: f.int("slots")?,
            }),
            "bye" => Self::Bye {
                reason: f.opt_str("reason")?.unwrap_or_default(),
            },
            other => Self::Unknown(other.to_owned()),
        })
    }
}

struct Fields<'a>(&'a Map<String, Value>);

impl Fields<'_> {
    fn str(&self, k: &str) -> Result<&str, WireError> {
        self.0
            .get(k)
            .and_then(Value::as_str)
            .ok_or_else(|| WireError::BadControl(format!("missing string \"{k}\"")))
    }
    fn opt_str(&self, k: &str) -> Result<Option<String>, WireError> {
        match self.0.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(_) => self.str(k).map(|s| Some(s.to_owned())),
        }
    }
    fn int<T: TryFrom<u64>>(&self, k: &str) -> Result<T, WireError> {
        self.0
            .get(k)
            .and_then(Value::as_u64)
            .and_then(|n| T::try_from(n).ok())
            .ok_or_else(|| {
                WireError::BadControl(format!("missing or out-of-range integer \"{k}\""))
            })
    }
    fn opt_int<T: TryFrom<u64>>(&self, k: &str) -> Result<Option<T>, WireError> {
        match self.0.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(_) => self.int(k).map(Some),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Link

/// Settings for [`Link::host`] and [`Link::guest`].
#[derive(Clone, Debug)]
pub struct LinkConfig {
    /// This side's game, sent in `hello`.
    pub game: String,
    /// Host: the TCP port to listen on (0 = any free port, see [`Link::control_addr`]).
    /// Guest: the host's port to connect to.
    pub control_port: u16,
    /// This side's UDP port (0 = any free port); announced to the peer in `hello`.
    pub udp_port: u16,
    pub heartbeat: Duration,
    /// Watchdog timeout; the effective one is at least 4 peer heartbeats.
    pub timeout: Duration,
    /// Guest: how often to retry connecting while no host answers.
    pub reconnect: Duration,
}

impl LinkConfig {
    pub fn new(game: impl Into<String>) -> Self {
        Self {
            game: game.into(),
            control_port: DEFAULT_CONTROL_PORT,
            udp_port: 0,
            heartbeat: Duration::from_millis(250),
            timeout: Duration::from_millis(2000),
            reconnect: Duration::from_millis(500),
        }
    }
}

/// The other side of an established session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerInfo {
    pub game: String,
    pub role: Role,
    /// The negotiated protocol version.
    pub protocol: u16,
    pub session: u32,
    pub udp_addr: SocketAddr,
    pub heartbeat: Duration,
    /// The watchdog timeout this side applies to the peer.
    pub timeout: Duration,
}

/// What [`Link::poll`] reports.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The handshake completed.
    Connected(PeerInfo),
    /// A session ended: the peer said bye, closed the connection, broke the protocol or went
    /// silent past the watchdog timeout. Focus is back with the host.
    Lost {
        reason: String,
    },
    /// A connection closed before the handshake completed.
    HandshakeFailed {
        reason: String,
    },
    Camera {
        header: Header,
        camera: Camera,
    },
    /// Poses newer than the last one of their entity.
    Poses {
        header: Header,
        poses: Vec<Pose>,
    },
    /// A complete collision snapshot, all chunks reassembled.
    Collision(Collision),
    Frame {
        header: Header,
        frame: FrameReady,
    },
    Spawn(Spawn),
    Despawn {
        entity: u32,
    },
    /// Guest: the host moved the focus. Host: the guest asked for it (grant with
    /// [`Link::set_focus`]).
    Focus {
        focus: Role,
        requested: bool,
    },
    FrameChannel(FrameChannel),
}

/// Counters for diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub datagrams_sent: u64,
    pub datagrams_received: u64,
    /// Datagrams from the peer that failed to decode.
    pub malformed: u64,
    /// Datagrams or records dropped as older than what was already delivered.
    pub stale: u64,
    /// Datagrams from another address, session or version.
    pub foreign: u64,
    /// Sends dropped because the socket buffer was full.
    pub send_dropped: u64,
    pub lines_sent: u64,
    pub lines_received: u64,
}

struct Ctl {
    stream: TcpStream,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
}

impl Ctl {
    fn new(stream: TcpStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        stream.set_nodelay(true)?;
        Ok(Self {
            stream,
            rbuf: Vec::new(),
            wbuf: Vec::new(),
        })
    }

    fn queue(&mut self, c: &Control) {
        self.wbuf.extend_from_slice(c.to_line().as_bytes());
    }

    /// Writes as much as the socket takes without blocking.
    fn flush(&mut self) -> Result<(), String> {
        while !self.wbuf.is_empty() {
            match self.stream.write(&self.wbuf) {
                Ok(0) => return Err("connection closed".into()),
                Ok(n) => {
                    self.wbuf.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("control write failed: {e}")),
            }
        }
        if self.wbuf.len() > MAX_PENDING_CONTROL {
            return Err("peer stopped reading the control channel".into());
        }
        Ok(())
    }

    /// Reads what is available and splits complete lines off. The error, if any, applies after
    /// the returned lines.
    fn read_lines(&mut self) -> (Vec<String>, Option<String>) {
        let mut lines = Vec::new();
        let mut buf = [0u8; 8192];
        let mut end = None;
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    end = Some("connection closed".to_owned());
                    break;
                }
                Ok(n) => self.rbuf.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    end = Some(format!("control read failed: {e}"));
                    break;
                }
            }
            if self.rbuf.len() > 4 * MAX_LINE {
                break; // Let the caller process what it has before reading on.
            }
        }
        let mut start = 0;
        while let Some(i) = self.rbuf[start..].iter().position(|&b| b == b'\n') {
            let line = &self.rbuf[start..start + i + 1];
            start += i + 1;
            if line.len() > MAX_LINE {
                return (lines, Some("control line too long".into()));
            }
            match std::str::from_utf8(line) {
                Ok(s) => lines.push(s.to_owned()),
                Err(_) => return (lines, Some("control line is not UTF-8".into())),
            }
        }
        self.rbuf.drain(..start);
        if self.rbuf.len() >= MAX_LINE {
            return (lines, Some("control line too long".into()));
        }
        (lines, end)
    }
}

enum Conn {
    /// No connection. A guest retries at `next_try`; a host waits for one.
    Idle { next_try: Instant },
    /// Connected, waiting for the peer's `hello`.
    Handshake { ctl: Ctl, since: Instant },
    Live {
        ctl: Ctl,
        peer: PeerInfo,
        last_heard: Instant,
    },
}

struct Assembly {
    query: u32,
    count: u16,
    center: [f32; 3],
    radius: f32,
    chunks: Vec<Option<Vec<Triangle>>>,
    received: u16,
}

/// One side of a passthrough link. Create it with [`Link::host`] or [`Link::guest`], then call
/// [`Link::poll`] once per frame or tick; it never blocks.
pub struct Link {
    role: Role,
    cfg: LinkConfig,
    listener: Option<TcpListener>,
    udp: UdpSocket,
    conn: Conn,
    focus: Role,
    seq: u32,
    next_query: u32,
    last_beat: Instant,
    events: Vec<Event>,
    stats: Stats,
    out: Vec<u8>,
    // Receive-side filters, reset per session.
    last_camera: Option<u32>,
    pose_times: HashMap<u32, u64>,
    last_collision: Option<u32>,
    assembly: Option<Assembly>,
    last_frame: u64,
}

fn localhost(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

fn millis(d: Duration) -> u32 {
    d.as_millis().min(u128::from(u32::MAX)) as u32
}

fn random_session() -> u32 {
    let mut h = RandomState::new().build_hasher();
    h.write_u64(now_us());
    h.write_u32(std::process::id());
    (h.finish() as u32).max(1)
}

impl Link {
    /// Listens on `127.0.0.1:control_port` and waits, in [`Link::poll`], for one guest at a time.
    pub fn host(cfg: LinkConfig) -> io::Result<Self> {
        let listener = TcpListener::bind(localhost(cfg.control_port))?;
        listener.set_nonblocking(true)?;
        Self::new(Role::Host, cfg, Some(listener))
    }

    /// Connects, in [`Link::poll`], to a host on `127.0.0.1:control_port`, retrying until one
    /// answers and reconnecting after a session ends.
    pub fn guest(cfg: LinkConfig) -> io::Result<Self> {
        Self::new(Role::Guest, cfg, None)
    }

    fn new(role: Role, cfg: LinkConfig, listener: Option<TcpListener>) -> io::Result<Self> {
        let udp = UdpSocket::bind(localhost(cfg.udp_port))?;
        udp.set_nonblocking(true)?;
        let now = Instant::now();
        Ok(Self {
            role,
            cfg,
            listener,
            udp,
            conn: Conn::Idle { next_try: now },
            focus: Role::Host,
            seq: 0,
            next_query: 1,
            last_beat: now,
            events: Vec::new(),
            stats: Stats::default(),
            out: Vec::with_capacity(MAX_DATAGRAM),
            last_camera: None,
            pose_times: HashMap::new(),
            last_collision: None,
            assembly: None,
            last_frame: 0,
        })
    }

    pub fn role(&self) -> Role {
        self.role
    }

    /// The host's listening address (useful with `control_port: 0`); `None` for a guest.
    pub fn control_addr(&self) -> Option<SocketAddr> {
        self.listener.as_ref().and_then(|l| l.local_addr().ok())
    }

    pub fn udp_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }

    /// The peer, once the handshake has completed.
    pub fn peer(&self) -> Option<&PeerInfo> {
        match &self.conn {
            Conn::Live { peer, .. } => Some(peer),
            _ => None,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.peer().is_some()
    }

    /// Which side currently has input focus.
    pub fn focus(&self) -> Role {
        self.focus
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Does all pending network work without blocking and returns what happened since the last
    /// call. Call it every frame; heartbeats are sent from here, so a side that stops polling
    /// looks dead to its peer, as intended.
    pub fn poll(&mut self) -> Vec<Event> {
        let now = Instant::now();
        self.poll_connect(now);
        self.poll_control(now);
        self.poll_udp(now);
        self.poll_timers(now);
        std::mem::take(&mut self.events)
    }

    fn hello(&self, protocol: u16, session: Option<u32>) -> Control {
        Control::Hello(Hello {
            role: self.role,
            game: self.cfg.game.clone(),
            protocol,
            min_protocol: MIN_PROTOCOL_VERSION,
            udp_port: self.udp.local_addr().map_or(0, |a| a.port()),
            heartbeat_ms: millis(self.cfg.heartbeat),
            timeout_ms: millis(self.cfg.timeout),
            session,
        })
    }

    fn poll_connect(&mut self, now: Instant) {
        if let Some(listener) = &self.listener {
            loop {
                let stream = match listener.accept() {
                    Ok((s, _)) => s,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => break,
                };
                if matches!(self.conn, Conn::Idle { .. }) {
                    if let Ok(ctl) = Ctl::new(stream) {
                        self.conn = Conn::Handshake { ctl, since: now };
                    }
                } else if let Ok(mut ctl) = Ctl::new(stream) {
                    ctl.queue(&Control::Bye {
                        reason: "busy".into(),
                    });
                    let _ = ctl.flush();
                }
            }
            return;
        }
        let Conn::Idle { next_try } = self.conn else {
            return;
        };
        if now < next_try {
            return;
        }
        let addr = localhost(self.cfg.control_port);
        match TcpStream::connect_timeout(&addr, Duration::from_millis(100)).and_then(Ctl::new) {
            Ok(mut ctl) => {
                ctl.queue(&self.hello(PROTOCOL_VERSION, None));
                self.conn = Conn::Handshake { ctl, since: now };
            }
            Err(_) => {
                self.conn = Conn::Idle {
                    next_try: now + self.cfg.reconnect,
                }
            }
        }
    }

    fn ctl(&mut self) -> Option<&mut Ctl> {
        match &mut self.conn {
            Conn::Handshake { ctl, .. } | Conn::Live { ctl, .. } => Some(ctl),
            Conn::Idle { .. } => None,
        }
    }

    fn poll_control(&mut self, now: Instant) {
        let Some(ctl) = self.ctl() else { return };
        if let Err(reason) = ctl.flush() {
            return self.end(reason, false);
        }
        let (lines, end) = ctl.read_lines();
        for line in lines {
            self.stats.lines_received += 1;
            if let Err((reason, bye)) = self.on_line(&line, now) {
                return self.end(reason, bye);
            }
            if matches!(self.conn, Conn::Idle { .. }) {
                return;
            }
        }
        if let Some(reason) = end {
            self.end(reason, false);
        }
    }

    /// Handles one control line; an error ends the session (with a `bye` when the flag is set).
    fn on_line(&mut self, line: &str, now: Instant) -> Result<(), (String, bool)> {
        let msg = Control::parse(line).map_err(|e| (format!("protocol error: {e}"), true))?;
        if let Conn::Live { last_heard, .. } = &mut self.conn {
            *last_heard = now;
        }
        if let Control::Bye { reason } = &msg {
            return Err((format!("peer said bye: {reason}"), false));
        }
        if matches!(self.conn, Conn::Handshake { .. }) {
            let Control::Hello(h) = msg else {
                return Err(("expected hello".into(), true));
            };
            return self.on_hello(h, now);
        }
        match msg {
            Control::Hello(_) => return Err(("second hello".into(), true)),
            Control::Spawn(s) => self.events.push(Event::Spawn(s)),
            Control::Despawn { entity } => {
                self.pose_times.remove(&entity);
                self.events.push(Event::Despawn { entity });
            }
            Control::Input { focus, .. } => {
                let requested = self.role == Role::Host;
                if !requested {
                    self.focus = focus;
                }
                self.events.push(Event::Focus { focus, requested });
            }
            Control::FrameChannel(c) => self.events.push(Event::FrameChannel(c)),
            Control::Bye { .. } | Control::Unknown(_) => {}
        }
        Ok(())
    }

    fn on_hello(&mut self, h: Hello, now: Instant) -> Result<(), (String, bool)> {
        let expected = match self.role {
            Role::Host => Role::Guest,
            Role::Guest => Role::Host,
        };
        if h.role != expected {
            return Err((format!("peer claims role {}", h.role.as_str()), true));
        }
        if h.udp_port == 0 {
            return Err(("peer announced udp_port 0".into(), true));
        }
        let (protocol, session) = match self.role {
            Role::Host => {
                let v = negotiate(
                    (MIN_PROTOCOL_VERSION, PROTOCOL_VERSION),
                    (h.min_protocol, h.protocol),
                )
                .ok_or_else(|| {
                    (
                        format!(
                            "unsupported protocol: peer {}..={}, ours {}..={}",
                            h.min_protocol, h.protocol, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION
                        ),
                        true,
                    )
                })?;
                (v, random_session())
            }
            Role::Guest => {
                if !(MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&h.protocol) {
                    return Err((
                        format!("host chose unsupported protocol {}", h.protocol),
                        true,
                    ));
                }
                let s = h
                    .session
                    .filter(|&s| s != 0)
                    .ok_or_else(|| ("host hello without a nonzero session".to_owned(), true))?;
                (h.protocol, s)
            }
        };
        let heartbeat = Duration::from_millis(u64::from(h.heartbeat_ms));
        let peer = PeerInfo {
            game: h.game,
            role: h.role,
            protocol,
            session,
            udp_addr: localhost(h.udp_port),
            heartbeat,
            timeout: self.cfg.timeout.max(heartbeat * 4),
        };
        let reply = (self.role == Role::Host).then(|| self.hello(protocol, Some(session)));
        let Conn::Handshake { mut ctl, .. } =
            std::mem::replace(&mut self.conn, Conn::Idle { next_try: now })
        else {
            unreachable!("on_hello is only called during the handshake")
        };
        if let Some(reply) = reply {
            ctl.queue(&reply);
            self.stats.lines_sent += 1;
            let _ = ctl.flush();
        }
        self.reset_session();
        self.conn = Conn::Live {
            ctl,
            peer: peer.clone(),
            last_heard: now,
        };
        self.last_beat = now;
        self.events.push(Event::Connected(peer));
        self.send(&Datagram::Heartbeat)
            .map_err(|e| (e.to_string(), true))?;
        Ok(())
    }

    fn reset_session(&mut self) {
        self.focus = Role::Host;
        self.last_camera = None;
        self.pose_times.clear();
        self.last_collision = None;
        self.assembly = None;
        self.last_frame = 0;
    }

    /// Ends the current connection, reporting it as lost (or as a failed handshake).
    fn end(&mut self, reason: String, bye: bool) {
        let now = Instant::now();
        let next_try = if self.role == Role::Guest {
            now + self.cfg.reconnect
        } else {
            now
        };
        let was_live = matches!(self.conn, Conn::Live { .. });
        if let Conn::Handshake { mut ctl, .. } | Conn::Live { mut ctl, .. } =
            std::mem::replace(&mut self.conn, Conn::Idle { next_try })
        {
            if bye {
                ctl.queue(&Control::Bye {
                    reason: reason.clone(),
                });
                let _ = ctl.flush();
            }
            let _ = ctl.stream.shutdown(std::net::Shutdown::Both);
        }
        self.reset_session();
        self.events.push(if was_live {
            Event::Lost { reason }
        } else {
            Event::HandshakeFailed { reason }
        });
    }

    fn poll_udp(&mut self, now: Instant) {
        let mut buf = [0u8; 2 * MAX_DATAGRAM];
        for _ in 0..4096 {
            let (n, from) = match self.udp.recv_from(&mut buf) {
                Ok(r) => r,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                // Windows reports ICMP port-unreachable from earlier sends here; skip it.
                Err(_) => continue,
            };
            let Conn::Live {
                peer, last_heard, ..
            } = &mut self.conn
            else {
                self.stats.foreign += 1;
                continue;
            };
            if from != peer.udp_addr {
                self.stats.foreign += 1;
                continue;
            }
            let (session, version) = (peer.session, peer.protocol);
            match decode_header(&buf[..n]) {
                Ok((h, _)) if h.session != session || h.version != version => {
                    self.stats.foreign += 1;
                    continue;
                }
                Ok(_) => {}
                Err(_) => {
                    self.stats.malformed += 1;
                    continue;
                }
            }
            let (header, body) = match decode_datagram(&buf[..n]) {
                Ok(d) => d,
                Err(_) => {
                    self.stats.malformed += 1;
                    continue;
                }
            };
            *last_heard = now;
            self.stats.datagrams_received += 1;
            self.on_datagram(header, body);
        }
    }

    fn on_datagram(&mut self, header: Header, body: Datagram) {
        match body {
            Datagram::Heartbeat | Datagram::Unknown { .. } => {}
            Datagram::Camera(camera) => {
                if self.last_camera.is_some_and(|l| !seq_newer(header.seq, l)) {
                    self.stats.stale += 1;
                    return;
                }
                self.last_camera = Some(header.seq);
                self.events.push(Event::Camera { header, camera });
            }
            Datagram::Pose(mut poses) => {
                let before = poses.len();
                poses.retain(|p| {
                    let last = self.pose_times.entry(p.entity).or_insert(0);
                    let fresh = p.time_us >= *last;
                    if fresh {
                        *last = p.time_us;
                    }
                    fresh
                });
                self.stats.stale += (before - poses.len()) as u64;
                if !poses.is_empty() {
                    self.events.push(Event::Poses { header, poses });
                }
            }
            Datagram::Collision(c) => self.on_collision(c),
            Datagram::Frame(frame) => {
                if frame.frame <= self.last_frame {
                    self.stats.stale += 1;
                    return;
                }
                self.last_frame = frame.frame;
                self.events.push(Event::Frame { header, frame });
            }
        }
    }

    fn on_collision(&mut self, c: CollisionChunk) {
        if self.last_collision.is_some_and(|l| !seq_newer(c.query, l)) {
            self.stats.stale += 1;
            return;
        }
        match &self.assembly {
            Some(a) if a.query == c.query => {
                if a.count != c.chunk_count {
                    self.stats.malformed += 1;
                    return;
                }
            }
            Some(a) if seq_newer(a.query, c.query) => {
                self.stats.stale += 1;
                return;
            }
            _ => {
                self.assembly = Some(Assembly {
                    query: c.query,
                    count: c.chunk_count,
                    center: c.center,
                    radius: c.radius,
                    chunks: vec![None; usize::from(c.chunk_count)],
                    received: 0,
                })
            }
        }
        let a = self.assembly.as_mut().expect("set above");
        let slot = &mut a.chunks[usize::from(c.chunk_index)];
        if slot.is_none() {
            *slot = Some(c.triangles);
            a.received += 1;
        }
        if a.received == a.count {
            let a = self.assembly.take().expect("set above");
            self.last_collision = Some(a.query);
            self.events.push(Event::Collision(Collision {
                query: a.query,
                center: a.center,
                radius: a.radius,
                triangles: a.chunks.into_iter().flatten().flatten().collect(),
            }));
        }
    }

    fn poll_timers(&mut self, now: Instant) {
        match &self.conn {
            Conn::Handshake { since, .. } if now.duration_since(*since) > self.cfg.timeout => {
                self.end("no hello before the timeout".into(), true)
            }
            Conn::Live {
                last_heard, peer, ..
            } => {
                let silent = now.duration_since(*last_heard);
                if silent > peer.timeout {
                    let ms = silent.as_millis();
                    return self.end(format!("watchdog: nothing heard for {ms} ms"), true);
                }
                if now.duration_since(self.last_beat) >= self.cfg.heartbeat {
                    self.last_beat = now;
                    let _ = self.send(&Datagram::Heartbeat);
                }
            }
            _ => {}
        }
    }

    /// Sends one datagram to the peer and returns its `seq`.
    fn send(&mut self, body: &Datagram) -> Result<u32, LinkError> {
        let Conn::Live { peer, .. } = &self.conn else {
            return Err(LinkError::NotConnected);
        };
        let addr = peer.udp_addr;
        let header = Header {
            version: peer.protocol,
            flags: 0,
            session: peer.session,
            seq: self.seq,
            time_us: now_us(),
        };
        encode_datagram(&header, body, &mut self.out)?;
        self.seq = self.seq.wrapping_add(1);
        match self.udp.send_to(&self.out, addr) {
            Ok(_) => self.stats.datagrams_sent += 1,
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock
                        | ErrorKind::ConnectionRefused
                        | ErrorKind::ConnectionReset
                ) =>
            {
                self.stats.send_dropped += 1
            }
            Err(e) => return Err(e.into()),
        }
        Ok(header.seq)
    }

    fn send_control(&mut self, c: &Control) -> Result<(), LinkError> {
        if !self.is_connected() {
            return Err(LinkError::NotConnected);
        }
        self.stats.lines_sent += 1;
        let ctl = self.ctl().expect("connected");
        ctl.queue(c);
        if let Err(reason) = ctl.flush() {
            self.end(reason, false);
            return Err(LinkError::NotConnected);
        }
        Ok(())
    }

    /// Sends the camera; returns the datagram's `seq`, which the peer quotes in
    /// [`FrameReady::camera_seq`].
    pub fn send_camera(&mut self, camera: &Camera) -> Result<u32, LinkError> {
        self.send(&Datagram::Camera(*camera))
    }

    /// Sends poses, [`MAX_POSES`] per datagram.
    pub fn send_poses(&mut self, poses: &[Pose]) -> Result<(), LinkError> {
        for chunk in poses.chunks(MAX_POSES) {
            self.send(&Datagram::Pose(chunk.to_vec()))?;
        }
        Ok(())
    }

    /// Sends a collision snapshot in as many chunks as it needs; returns its query number.
    pub fn send_collision(
        &mut self,
        center: [f32; 3],
        radius: f32,
        triangles: &[Triangle],
    ) -> Result<u32, LinkError> {
        let c = Collision {
            query: self.next_query,
            center,
            radius,
            triangles: triangles.to_vec(),
        };
        for chunk in c.chunks()? {
            self.send(&Datagram::Collision(chunk))?;
        }
        self.next_query = self.next_query.wrapping_add(1);
        Ok(c.query)
    }

    /// Announces a frame written with [`FrameWriter::write`].
    pub fn send_frame(&mut self, frame: &FrameReady) -> Result<(), LinkError> {
        self.send(&Datagram::Frame(*frame)).map(|_| ())
    }

    pub fn spawn(&mut self, spawn: &Spawn) -> Result<(), LinkError> {
        self.send_control(&Control::Spawn(spawn.clone()))
    }

    pub fn despawn(&mut self, entity: u32) -> Result<(), LinkError> {
        self.send_control(&Control::Despawn { entity })
    }

    /// Host: gives the focus to `focus` (takes effect at once). Guest: asks the host for it.
    pub fn set_focus(&mut self, focus: Role) -> Result<(), LinkError> {
        self.send_control(&Control::Input {
            focus,
            reason: None,
        })?;
        if self.role == Role::Host {
            self.focus = focus;
        }
        Ok(())
    }

    /// Tells the peer about a pixel file, see [`FrameWriter::channel`].
    pub fn announce_frames(&mut self, channel: &FrameChannel) -> Result<(), LinkError> {
        self.send_control(&Control::FrameChannel(channel.clone()))
    }

    /// Ends the session with a `bye`. A guest reconnects after [`LinkConfig::reconnect`].
    pub fn disconnect(&mut self, reason: &str) {
        if !matches!(self.conn, Conn::Idle { .. }) {
            self.end(reason.to_owned(), true);
            self.events.pop(); // The caller knows; don't report its own disconnect back.
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        if let Some(ctl) = self.ctl() {
            ctl.queue(&Control::Bye {
                reason: "closing".into(),
            });
            let _ = ctl.flush();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Pixel file

const FILE_HEADER_LEN: usize = 64;
const SLOT_HEADER_LEN: usize = 64;
const FRAME_MAGIC: &[u8; 8] = b"RBFRAME\0";
/// `format` value for RGBA8, sRGB, straight alpha, rows top to bottom.
pub const FORMAT_RGBA8_SRGB: u32 = 1;
pub const MAX_SLOTS: u32 = 16;

fn slot_size(stride: u32, height: u32) -> u64 {
    (SLOT_HEADER_LEN as u64 + u64::from(stride) * u64::from(height)).div_ceil(64) * 64
}

/// A pixel frame read back from a [`FrameReader`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameData {
    pub frame: u64,
    pub time_us: u64,
    pub camera_seq: u32,
    pub width: u32,
    pub height: u32,
}

/// Why [`FrameReader::read`] returned no frame. All of them mean "wait for the next one".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameMiss {
    /// The slot or size in the datagram does not fit the file.
    OutOfRange,
    /// The writer was in the slot, or overwrote it while it was copied.
    Torn,
    /// The slot already holds another frame.
    Replaced,
}

/// Writes RGBA frames into a memory-mapped pixel file (contract section 9.6).
pub struct FrameWriter {
    map: MmapMut,
    path: PathBuf,
    channel: FrameChannel,
    stride: u32,
    slot_size: u64,
    next_frame: u64,
}

impl FrameWriter {
    /// Creates (or truncates) the file at `path` with room for `slots` frames of up to
    /// `width` x `height` pixels.
    pub fn create(path: &Path, width: u32, height: u32, slots: u32) -> io::Result<Self> {
        let invalid = |m: &str| io::Error::new(ErrorKind::InvalidInput, m.to_owned());
        if !(1..=MAX_SLOTS).contains(&slots) || width == 0 || height == 0 {
            return Err(invalid("slots must be 1 to 16 and the size nonzero"));
        }
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| invalid("width too large"))?;
        let slot_size = slot_size(stride, height);
        let len = FILE_HEADER_LEN as u64 + slot_size * u64::from(slots);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(len)?;
        // SAFETY: we just created the file and only this writer maps it writable.
        let mut map = unsafe { MmapMut::map_mut(&file)? };
        let mut h = Vec::with_capacity(FILE_HEADER_LEN);
        h.extend_from_slice(FRAME_MAGIC);
        let mut w = Out(&mut h);
        w.u32(1);
        w.u32(slots);
        w.u32(width);
        w.u32(height);
        w.u32(stride);
        w.u32(FORMAT_RGBA8_SRGB);
        w.u64(slot_size);
        h.resize(FILE_HEADER_LEN, 0);
        map[..FILE_HEADER_LEN].copy_from_slice(&h);
        let path = std::path::absolute(path)?;
        Ok(Self {
            map,
            channel: FrameChannel {
                path: path.to_string_lossy().into_owned(),
                width,
                height,
                slots,
            },
            path,
            stride,
            slot_size,
            next_frame: 1,
        })
    }

    /// Creates the file in the temp directory, named after this process and `tag`.
    pub fn create_temp(tag: &str, width: u32, height: u32, slots: u32) -> io::Result<Self> {
        let name = format!("rewrite-bridge-{}-{tag}.frames", std::process::id());
        Self::create(&std::env::temp_dir().join(name), width, height, slots)
    }

    /// What to pass to [`Link::announce_frames`].
    pub fn channel(&self) -> &FrameChannel {
        &self.channel
    }

    /// Writes one frame of tightly packed RGBA rows into the next slot and returns the
    /// announcement to send with [`Link::send_frame`].
    pub fn write(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        camera_seq: u32,
        time_us: u64,
    ) -> Result<FrameReady, WireError> {
        if width > self.channel.width || height > self.channel.height {
            return Err(WireError::BadValue("frame larger than the pixel file"));
        }
        let row = width as usize * 4;
        if rgba.len() != row * height as usize {
            return Err(WireError::BadValue("rgba length is not width * height * 4"));
        }
        let frame = self.next_frame;
        self.next_frame += 1;
        let slot = (frame % u64::from(self.channel.slots)) as u32;
        let base = FILE_HEADER_LEN + (self.slot_size * u64::from(slot)) as usize;
        let mut h = Vec::with_capacity(SLOT_HEADER_LEN - 8);
        let mut w = Out(&mut h);
        w.u64(frame);
        w.u64(time_us);
        w.u32(camera_seq);
        w.u32(width);
        w.u32(height);
        h.resize(SLOT_HEADER_LEN - 8, 0);
        // All access goes through this one pointer, so the atomic lock word and the plain bytes
        // never alias a Rust slice of the whole map.
        let ptr = self.map.as_mut_ptr();
        let lock = lock_at(ptr, base);
        let odd = lock.load(Ordering::Relaxed).wrapping_add(1) | 1;
        lock.store(odd, Ordering::Relaxed);
        fence(Ordering::Release);
        let stride = self.stride as usize;
        // SAFETY: every range is inside the slot, which `create` sized for width x height.
        unsafe {
            std::ptr::copy_nonoverlapping(h.as_ptr(), ptr.add(base + 8), h.len());
            for y in 0..height as usize {
                let at = base + SLOT_HEADER_LEN + y * stride;
                std::ptr::copy_nonoverlapping(rgba.as_ptr().add(y * row), ptr.add(at), row);
            }
        }
        lock.store(odd.wrapping_add(1), Ordering::Release);
        Ok(FrameReady {
            frame,
            slot,
            camera_seq,
            width,
            height,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FrameWriter {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The slot's seqlock word. The map is page-aligned and every slot starts at a multiple of 64.
fn lock_at<'a, T>(map: *const T, base: usize) -> &'a AtomicU64 {
    // SAFETY: `base` is in bounds and 8-byte aligned (see above); the word is only ever
    // accessed atomically, by this process and the peer.
    unsafe { &*((map as *const u8).add(base) as *const AtomicU64) }
}

#[allow(clippy::type_complexity)]
fn parse_file_header(b: &[u8]) -> Result<([u8; 8], u32, u32, u32, u32, u32, u32, u64), WireError> {
    let mut r = In { b, at: 0 };
    Ok((
        r.bytes()?,
        r.u32()?,
        r.u32()?,
        r.u32()?,
        r.u32()?,
        r.u32()?,
        r.u32()?,
        r.u64()?,
    ))
}

/// Reads frames from a peer's pixel file, mapped read-only.
pub struct FrameReader {
    map: Mmap,
    slots: u32,
    width: u32,
    height: u32,
    stride: u32,
    slot_size: u64,
}

impl FrameReader {
    /// Maps the file a `frame_channel` message named and checks its header.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        // SAFETY: the file is shared with its writer; this map is only read through raw
        // pointers and the seqlock below, never as a Rust slice that could change under it.
        let map = unsafe { Mmap::map(&file)? };
        let bad = |m: &str| io::Error::new(ErrorKind::InvalidData, m.to_owned());
        if map.len() < FILE_HEADER_LEN {
            return Err(bad("pixel file too short"));
        }
        let (magic, version, slots, width, height, stride, format, slot_sz) =
            parse_file_header(&map[..FILE_HEADER_LEN]).map_err(|_| bad("header"))?;
        if &magic != FRAME_MAGIC || version != 1 || format != FORMAT_RGBA8_SRGB {
            return Err(bad("not a v1 RGBA8 pixel file"));
        }
        if !(1..=MAX_SLOTS).contains(&slots)
            || u64::from(stride) < u64::from(width) * 4
            || slot_sz < slot_size(stride, height)
            || slot_sz % 64 != 0
        {
            return Err(bad("inconsistent pixel file header"));
        }
        let need = slot_sz
            .checked_mul(u64::from(slots))
            .and_then(|n| n.checked_add(FILE_HEADER_LEN as u64));
        if need.is_none_or(|n| (map.len() as u64) < n) {
            return Err(bad("pixel file shorter than its header says"));
        }
        Ok(Self {
            map,
            slots,
            width,
            height,
            stride,
            slot_size: slot_sz,
        })
    }

    /// Copies the announced frame into `rgba` as tightly packed rows, if the slot still holds it
    /// intact.
    pub fn read(&self, ready: &FrameReady, rgba: &mut Vec<u8>) -> Result<FrameData, FrameMiss> {
        if ready.slot >= self.slots || ready.width > self.width || ready.height > self.height {
            return Err(FrameMiss::OutOfRange);
        }
        let base = FILE_HEADER_LEN + (self.slot_size * u64::from(ready.slot)) as usize;
        let ptr = self.map.as_ptr();
        let lock = lock_at(ptr, base);
        let l1 = lock.load(Ordering::Acquire);
        if l1 & 1 == 1 {
            return Err(FrameMiss::Torn);
        }
        let mut h = [0u8; SLOT_HEADER_LEN - 8];
        // SAFETY: in bounds (checked in `open`); the writer may change these bytes, which the
        // seqlock check below detects.
        unsafe { std::ptr::copy_nonoverlapping(ptr.add(base + 8), h.as_mut_ptr(), h.len()) };
        let mut r = In { b: &h, at: 0 };
        let data = FrameData {
            frame: r.u64().unwrap_or(0),
            time_us: r.u64().unwrap_or(0),
            camera_seq: r.u32().unwrap_or(0),
            width: r.u32().unwrap_or(0),
            height: r.u32().unwrap_or(0),
        };
        if data.frame != ready.frame || data.width != ready.width || data.height != ready.height {
            return Err(if data.frame > ready.frame {
                FrameMiss::Replaced
            } else {
                FrameMiss::Torn
            });
        }
        let row = data.width as usize * 4;
        rgba.clear();
        rgba.resize(row * data.height as usize, 0);
        for y in 0..data.height as usize {
            let at = base + SLOT_HEADER_LEN + y * self.stride as usize;
            // SAFETY: as above.
            unsafe {
                std::ptr::copy_nonoverlapping(ptr.add(at), rgba.as_mut_ptr().add(y * row), row)
            };
        }
        fence(Ordering::Acquire);
        if lock.load(Ordering::Relaxed) != l1 {
            return Err(FrameMiss::Torn);
        }
        Ok(data)
    }
}

// ---------------------------------------------------------------------------------------------
// Small vector helpers for the camera

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(a: [f32; 3]) -> [f32; 3] {
    let l = dot(a, a).sqrt();
    if l > 0.0 {
        [a[0] / l, a[1] / l, a[2] / l]
    } else {
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(seq: u32) -> Header {
        Header {
            version: 1,
            flags: 0,
            session: 0xABCD_0001,
            seq,
            time_us: 1_700_000_000_000_000,
        }
    }

    fn enc(body: &Datagram) -> Vec<u8> {
        let mut out = Vec::new();
        encode_datagram(&header(4), body, &mut out).unwrap();
        out
    }

    fn roundtrip(body: Datagram) -> Vec<u8> {
        let bytes = enc(&body);
        assert!(bytes.len() <= MAX_DATAGRAM);
        assert_eq!(decode_datagram(&bytes), Ok((header(4), body)));
        bytes
    }

    fn f32_at(b: &[u8], at: usize) -> f32 {
        f32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }
    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }
    fn u64_at(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }

    fn camera() -> Camera {
        let mut c = Camera::look_at(
            [1.0, 2.0, 3.0],
            [0.0; 3],
            1.0,
            0.1,
            f32::INFINITY,
            [1280, 720],
        );
        c.depth_range = DepthRange::Reversed;
        c
    }

    fn pose(entity: u32) -> Pose {
        Pose {
            entity,
            flags: POSE_TELEPORT,
            time_us: 123_456_789 + u64::from(entity),
            translation: [1.0, 2.0, 3.0],
            rotation: [0.0, 0.70710677, 0.0, 0.70710677],
            scale: [1.0, 1.5, 2.0],
            velocity: [4.0, 5.0, 6.0],
            angular_velocity: [7.0, 8.0, 9.0],
        }
    }

    fn tri(i: usize) -> Triangle {
        let x = i as f32;
        [[x, 0.0, 0.0], [x + 1.0, 0.0, 0.0], [x, 0.0, -1.0]]
    }

    fn samples() -> Vec<Datagram> {
        vec![
            Datagram::Heartbeat,
            Datagram::Camera(camera()),
            Datagram::Pose(vec![pose(1)]),
            Datagram::Pose((0..MAX_POSES as u32).map(pose).collect()),
            Datagram::Collision(CollisionChunk {
                query: 9,
                chunk_index: 1,
                chunk_count: 3,
                center: [1.0, 2.0, 3.0],
                radius: 25.0,
                triangles: (0..MAX_TRIANGLES).map(tri).collect(),
            }),
            Datagram::Collision(CollisionChunk {
                query: 10,
                chunk_index: 0,
                chunk_count: 1,
                center: [0.0; 3],
                radius: 5.0,
                triangles: vec![],
            }),
            Datagram::Frame(FrameReady {
                frame: 77,
                slot: 2,
                camera_seq: 41,
                width: 640,
                height: 360,
            }),
        ]
    }

    #[test]
    fn limits_match_the_contract() {
        assert_eq!(MAX_POSES, 102);
        assert_eq!(MAX_TRIANGLES, 226);
        assert_eq!(&MAGIC.to_le_bytes(), b"RBLK");
    }

    #[test]
    fn heartbeat_matches_the_contract_example() {
        let bytes = roundtrip(Datagram::Heartbeat);
        let expected = [
            0x52, 0x42, 0x4C, 0x4B, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0xCD, 0xAB, 0x04, 0x00,
            0x00, 0x00, 0x00, 0x40, 0x1E, 0x18, 0x24, 0x0A, 0x06, 0x00,
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn camera_layout() {
        let c = camera();
        let b = roundtrip(Datagram::Camera(c));
        assert_eq!(b.len(), HEADER_LEN + CAMERA_LEN);
        assert_eq!(b[6], KIND_CAMERA);
        let p = HEADER_LEN;
        assert_eq!(f32_at(&b, p + 12 * 4), c.view[12]);
        assert_eq!(f32_at(&b, p + 64 + 11 * 4), -1.0);
        assert_eq!(f32_at(&b, p + 128), 1.0);
        assert_eq!(f32_at(&b, p + 132), 0.1);
        assert_eq!(f32_at(&b, p + 136), f32::INFINITY);
        assert_eq!(u32_at(&b, p + 140), 1280);
        assert_eq!(u32_at(&b, p + 144), 720);
        assert_eq!(u32_at(&b, p + 148), 2);
    }

    #[test]
    fn pose_layout() {
        let b = roundtrip(Datagram::Pose(vec![pose(1), pose(2)]));
        assert_eq!(b.len(), HEADER_LEN + POSE_HEADER_LEN + 2 * POSE_LEN);
        assert_eq!(u32_at(&b, 24), 2);
        let r = HEADER_LEN + POSE_HEADER_LEN + POSE_LEN; // second record
        assert_eq!(u32_at(&b, r), 2);
        assert_eq!(u32_at(&b, r + 4), POSE_TELEPORT);
        assert_eq!(u64_at(&b, r + 8), 123_456_791);
        assert_eq!(f32_at(&b, r + 16), 1.0);
        assert_eq!(f32_at(&b, r + 28 + 12), 0.70710677);
        assert_eq!(f32_at(&b, r + 44 + 4), 1.5);
        assert_eq!(f32_at(&b, r + 56 + 8), 6.0);
        assert_eq!(f32_at(&b, r + 68 + 8), 9.0);
        let full = roundtrip(Datagram::Pose((0..MAX_POSES as u32).map(pose).collect()));
        assert!(full.len() <= MAX_DATAGRAM);
        let mut out = Vec::new();
        for n in [0, MAX_POSES + 1] {
            let poses = (0..n as u32).map(pose).collect();
            assert!(encode_datagram(&header(0), &Datagram::Pose(poses), &mut out).is_err());
        }
    }

    #[test]
    fn collision_layout_and_chunking() {
        let b = roundtrip(samples().swap_remove(4));
        assert_eq!(
            b.len(),
            MAX_DATAGRAM,
            "a full chunk fills the datagram exactly"
        );
        let p = HEADER_LEN;
        assert_eq!(u32_at(&b, p), 9);
        assert_eq!(u16::from_le_bytes([b[p + 4], b[p + 5]]), 1);
        assert_eq!(u16::from_le_bytes([b[p + 6], b[p + 7]]), 3);
        assert_eq!(f32_at(&b, p + 16), 3.0);
        assert_eq!(f32_at(&b, p + 20), 25.0);
        assert_eq!(u32_at(&b, p + 24), MAX_TRIANGLES as u32);
        let t = p + COLLISION_HEADER_LEN + 36; // second triangle, vertex b, x
        assert_eq!(f32_at(&b, t + 12), 2.0);

        let whole = Collision {
            query: 3,
            center: [0.0; 3],
            radius: 10.0,
            triangles: (0..500).map(tri).collect(),
        };
        let chunks = whole.chunks().unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(
            chunks.iter().map(|c| c.triangles.len()).collect::<Vec<_>>(),
            [226, 226, 48]
        );
        let back: Vec<Triangle> = chunks
            .into_iter()
            .flat_map(|c| {
                roundtrip(Datagram::Collision(c.clone()))
                    .len()
                    .le(&MAX_DATAGRAM)
                    .then_some(c.triangles)
            })
            .flatten()
            .collect();
        assert_eq!(back, whole.triangles);
        let empty = Collision {
            triangles: vec![],
            ..whole
        };
        assert_eq!(empty.chunks().unwrap().len(), 1);
    }

    #[test]
    fn frame_layout() {
        let b = roundtrip(samples().swap_remove(6));
        assert_eq!(b.len(), HEADER_LEN + FRAME_LEN);
        assert_eq!(u64_at(&b, 24), 77);
        assert_eq!(u32_at(&b, 24 + 8), 2);
        assert_eq!(u32_at(&b, 24 + 12), 41);
        assert_eq!(u32_at(&b, 24 + 16), 640);
        assert_eq!(u32_at(&b, 24 + 20), 360);
        let zero = Datagram::Frame(FrameReady {
            frame: 0,
            slot: 0,
            camera_seq: 0,
            width: 1,
            height: 1,
        });
        assert!(encode_datagram(&header(0), &zero, &mut Vec::new()).is_err());
    }

    #[test]
    fn header_checks() {
        let good = enc(&Datagram::Heartbeat);
        let mut b = good.clone();
        b[0] = b'X';
        assert!(matches!(decode_datagram(&b), Err(WireError::BadMagic(_))));
        let mut b = good.clone();
        b[4] = 2;
        assert_eq!(decode_datagram(&b), Err(WireError::BadVersion(2)));
        let mut b = good.clone();
        b.push(0);
        assert!(matches!(
            decode_datagram(&b),
            Err(WireError::BadLength { .. })
        ));
        let mut b = good.clone();
        b[6] = 200;
        b.extend_from_slice(&[1, 2, 3]);
        assert_eq!(
            decode_datagram(&b).unwrap().1,
            Datagram::Unknown {
                kind: 200,
                payload_len: 3
            }
        );
        let mut b = enc(&Datagram::Camera(camera()));
        b[24 + 148] = 3;
        assert_eq!(decode_datagram(&b), Err(WireError::BadValue("depth_range")));
    }

    #[test]
    fn control_lines_round_trip() {
        let msgs = [
            Control::Hello(Hello {
                role: Role::Host,
                game: "War Thunder \"rewrite\" ✈".into(),
                protocol: 1,
                min_protocol: 1,
                udp_port: 53000,
                heartbeat_ms: 250,
                timeout_ms: 2000,
                session: Some(u32::MAX),
            }),
            Control::Hello(Hello {
                role: Role::Guest,
                game: "g".into(),
                protocol: 3,
                min_protocol: 1,
                udp_port: 1,
                heartbeat_ms: 1,
                timeout_ms: 1,
                session: None,
            }),
            Control::Spawn(Spawn {
                entity: u32::MAX,
                provider: "Warframe".into(),
                asset: "/Lotus/Characters/Excalibur".into(),
                name: Some("Excalibur".into()),
            }),
            Control::Spawn(Spawn {
                entity: 0,
                provider: "p".into(),
                asset: "a\nb".into(),
                name: None,
            }),
            Control::Despawn { entity: 7 },
            Control::Input {
                focus: Role::Guest,
                reason: Some("menu".into()),
            },
            Control::Input {
                focus: Role::Host,
                reason: None,
            },
            Control::FrameChannel(FrameChannel {
                path: "C:\\Temp\\x.frames".into(),
                width: 1280,
                height: 720,
                slots: 3,
            }),
            Control::Bye {
                reason: "shutting down".into(),
            },
            Control::Unknown("future_thing".into()),
        ];
        for m in msgs {
            let line = m.to_line();
            assert!(
                line.ends_with('\n') && line.matches('\n').count() == 1,
                "{line}"
            );
            assert_eq!(Control::parse(&line), Ok(m));
        }
    }

    #[test]
    fn control_lines_as_another_implementation_writes_them() {
        let hello = r#"{"session":5,"type":"hello","role":"host","game":"SKSE","protocol":1,"min_protocol":1,"udp_port":9,"heartbeat_ms":100,"timeout_ms":900,"extra":[1,2]}"#;
        assert!(matches!(
            Control::parse(&format!("{hello}\r\n")),
            Ok(Control::Hello(Hello {
                session: Some(5),
                ..
            }))
        ));
        assert_eq!(
            Control::parse(r#"{"type":"bye"}"#),
            Ok(Control::Bye {
                reason: String::new()
            })
        );
        for bad in [
            "",
            "null",
            "[]",
            "{}",
            r#"{"type":5}"#,
            r#"{"type":"despawn"}"#,
            r#"{"type":"despawn","entity":4294967296}"#,
            r#"{"type":"despawn","entity":-1}"#,
            r#"{"type":"despawn","entity":1.5}"#,
            r#"{"type":"input","focus":"nobody"}"#,
            r#"{"type":"hello","role":"guest","game":"g","protocol":70000,"min_protocol":1,"udp_port":1,"heartbeat_ms":1,"timeout_ms":1}"#,
            r#"{"type":"spawn","entity":1,"provider":"p","asset":"a","name":3}"#,
            "{\"type\":\"bye\"",
        ] {
            assert!(Control::parse(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn version_negotiation() {
        assert_eq!(negotiate((1, 1), (1, 1)), Some(1));
        assert_eq!(negotiate((1, 3), (1, 2)), Some(2));
        assert_eq!(negotiate((1, 1), (2, 2)), None);
        assert_eq!(negotiate((2, 4), (1, 1)), None);
    }

    #[test]
    fn serial_numbers_wrap() {
        assert!(seq_newer(1, 0));
        assert!(seq_newer(0, u32::MAX));
        assert!(!seq_newer(u32::MAX, 0));
        assert!(!seq_newer(5, 5));
    }

    #[test]
    fn camera_helpers() {
        let c = Camera::look_at(
            [0.0, 2.0, 10.0],
            [0.0, 2.0, 0.0],
            1.0,
            0.1,
            100.0,
            [100, 100],
        );
        let eye = c.eye();
        let fwd = c.forward();
        for (a, b) in eye.iter().zip([0.0, 2.0, 10.0]) {
            assert!((a - b).abs() < 1e-5, "{eye:?}");
        }
        for (a, b) in fwd.iter().zip([0.0, 0.0, -1.0]) {
            assert!((a - b).abs() < 1e-5, "{fwd:?}");
        }
    }

    // ----- Robustness: garbage must be an error, never a panic.

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn every_truncation_is_an_error() {
        for body in samples() {
            let b = enc(&body);
            for n in 0..b.len() {
                assert!(
                    decode_datagram(&b[..n]).is_err(),
                    "{:?} cut to {n}",
                    body.kind()
                );
            }
        }
    }

    #[test]
    fn mutated_datagrams_never_panic() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let samples: Vec<Vec<u8>> = samples().iter().map(enc).collect();
        let mut accepted = 0;
        for i in 0..200_000 {
            let mut b = samples[i % samples.len()].clone();
            match rng.next() % 4 {
                0 => {
                    let at = rng.next() as usize % b.len();
                    b[at] ^= 1 << (rng.next() % 8);
                }
                1 => {
                    for _ in 0..1 + rng.next() % 8 {
                        let at = rng.next() as usize % b.len();
                        b[at] = rng.next() as u8;
                    }
                }
                2 => {
                    // Keep a valid header, randomize the rest.
                    let len = HEADER_LEN + rng.next() as usize % 600;
                    b.resize(len, 0);
                    for x in &mut b[HEADER_LEN..] {
                        *x = rng.next() as u8;
                    }
                    b[6] = 1 + (rng.next() % 6) as u8;
                }
                _ => {
                    let len = rng.next() as usize % 300;
                    b = (0..len).map(|_| rng.next() as u8).collect();
                }
            }
            if let Ok((h, d)) = decode_datagram(&b) {
                accepted += 1;
                // Whatever decodes must re-encode to the same bytes (unless unknown kind).
                if !matches!(d, Datagram::Unknown { .. }) {
                    // ... and re-encode to the same length (reserved bytes are written as zero).
                    let mut again = Vec::new();
                    encode_datagram(&h, &d, &mut again).unwrap();
                    assert_eq!(again.len(), b.len());
                }
            }
        }
        assert!(accepted > 0);
    }

    #[test]
    fn garbage_control_lines_never_panic() {
        let mut rng = Rng(42);
        let good = Control::Spawn(Spawn {
            entity: 1,
            provider: "p".into(),
            asset: "a".into(),
            name: None,
        })
        .to_line();
        for _ in 0..50_000 {
            let mut b = good.clone().into_bytes();
            for _ in 0..1 + rng.next() % 4 {
                let at = rng.next() as usize % b.len();
                b[at] = rng.next() as u8;
            }
            let s = String::from_utf8_lossy(&b);
            let _ = Control::parse(&s);
        }
        assert!(Control::parse(&"[".repeat(100_000)).is_err());
    }

    // ----- Pixel file

    #[test]
    fn frames_round_trip_through_the_file() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("rewrite-bridge-test-{}.frames", std::process::id()));
        let mut w = FrameWriter::create(&path, 8, 4, 3).unwrap();
        let r = FrameReader::open(Path::new(&w.channel().path)).unwrap();
        let mut out = Vec::new();
        for n in 1..=5u8 {
            let img: Vec<u8> = (0..6 * 3 * 4).map(|i| (i as u8).wrapping_mul(n)).collect();
            let ready = w.write(&img, 6, 3, u32::from(n) * 10, 99).unwrap();
            assert_eq!(ready.slot, (u64::from(n) % 3) as u32);
            let data = r.read(&ready, &mut out).unwrap();
            assert_eq!(out, img);
            assert_eq!(
                data,
                FrameData {
                    frame: u64::from(n),
                    time_us: 99,
                    camera_seq: u32::from(n) * 10,
                    width: 6,
                    height: 3,
                }
            );
        }
        // Frame 2 lived in slot 2, now overwritten by frame 5.
        let old = FrameReady {
            frame: 2,
            slot: 2,
            camera_seq: 20,
            width: 6,
            height: 3,
        };
        assert_eq!(r.read(&old, &mut out), Err(FrameMiss::Replaced));
        let past = FrameReady { slot: 3, ..old };
        assert_eq!(r.read(&past, &mut out), Err(FrameMiss::OutOfRange));
        // A writer caught mid-frame: the lock word is odd.
        let ready = w.write(&[0; 8 * 4 * 4], 8, 4, 0, 0).unwrap();
        let base = FILE_HEADER_LEN + (w.slot_size * u64::from(ready.slot)) as usize;
        lock_at(w.map.as_mut_ptr(), base).fetch_add(1, Ordering::SeqCst);
        assert_eq!(r.read(&ready, &mut out), Err(FrameMiss::Torn));
        assert!(w.write(&[0; 9 * 4 * 4], 9, 4, 0, 0).is_err());
        assert!(w.write(&[0; 3], 1, 1, 0, 0).is_err());
        drop(r);
        drop(w);
        assert!(!path.exists(), "the writer removes its file");
    }

    #[test]
    fn garbage_pixel_files_are_rejected() {
        let path =
            std::env::temp_dir().join(format!("rewrite-bridge-junk-{}.frames", std::process::id()));
        let mut rng = Rng(7);
        for len in [0usize, 10, 64, 200, 5000] {
            let junk: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            std::fs::write(&path, &junk).unwrap();
            assert!(FrameReader::open(&path).is_err());
        }
        // A valid header that claims more slots than the file holds. The writer's file is copied
        // and cut short, since Windows refuses to resize a file while it is mapped.
        let valid = {
            let w = FrameWriter::create(&path, 4, 4, 2).unwrap();
            std::fs::read(w.path()).unwrap()
        };
        std::fs::write(&path, &valid[..100]).unwrap();
        assert!(FrameReader::open(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
