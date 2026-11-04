//! Passthrough link: exchange camera, poses, collision and frames with another process on
//! 127.0.0.1, so a rewrite can act as the host or the guest of a two-game "passthrough" mod.
//!
//! `docs/CONTRACT.md` defines the protocol; this module is one implementation of it. It has three
//! layers, usable separately:
//!
//! - the wire codec: [`encode_datagram`] / [`decode_datagram`] for the binary UDP datagrams and
//!   [`Control::to_line`] / [`Control::parse`] for the JSON-lines control channel;
//! - [`Link`], which owns the sockets, does the handshake, heartbeat and watchdog, and is driven by
//!   a [`Link::poll`] that does not wait on the network, from a game loop or a Bevy system (but see the
//!   guest's connect on a target with no threads, below);
//! - [`FrameWriter`] / [`FrameReader`] for RGBA frames, and optionally their linear depth, in a
//!   memory-mapped file; [`compose`] has the reference CPU reprojection and depth compositing for
//!   such frames.
//!
//! A host can require a token from its guest (contract section 6.2): set [`LinkConfig::token`], or
//! [`LinkConfig::endpoint_file`] to have the host generate one and hand it, with the port, to the guest
//! through an owner-only [`Endpoint`] file.
//!
//! The link also runs in a component on `wasm32-wasip2`, which has no threads and no memory-mapped files:
//! a guest connects inside [`Link::poll`], which can wait up to 100 ms for it (a host that never accepts), and
//! the pixel file is an `Unsupported` error there.
//!
//! The link only exchanges what each process chooses to publish. It never touches another process.

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::mem::ManuallyDrop;
#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
use std::net::UdpSocket;
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use memmap2::{Mmap, MmapMut};
use serde_json::{json, Map, Value};

pub mod compose;

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
/// Most chunks one collision snapshot may have, and so `chunk_count` at most: 4096 chunks are 925696
/// triangles, 32 MiB (contract section 9.5). A decoder drops a chunk that claims more.
pub const MAX_COLLISION_CHUNKS: u16 = 4096;
/// Deepest nesting of arrays and objects in a control line, the line's own object counting as the
/// first level (contract section 8, "Syntax of a line"). `Control::parse` enforces it on the text,
/// before serde_json sees the line (whose own recursion limit happens to be the same).
pub const MAX_JSON_NESTING: usize = 127;
/// Longest control line, newline included.
pub const MAX_LINE: usize = 65536;
/// Unsent control bytes beyond which the peer counts as lost.
pub const MAX_PENDING_CONTROL: usize = 1 << 20;
/// How long a closing control connection is drained after its `bye` (contract section 6).
pub const LINGER: Duration = Duration::from_millis(200);
/// Largest width or height of a pixel file that a reader accepts (contract section 9.6).
pub const MAX_FRAME_DIM: u32 = 16384;
/// Largest `width * height` of a pixel file, in pixels (2^26, 8192 x 8192): a read then copies at
/// most 256 MiB of colour and 256 MiB of depth (contract section 9.6).
pub const MAX_FRAME_PIXELS: u64 = 1 << 26;
/// Live entities a receiver keeps for its peer; a `spawn` beyond this is ignored (section 8).
pub const MAX_ENTITIES: usize = 1024;
/// The range of `heartbeat_ms` a `hello` may announce (section 8).
pub const HEARTBEAT_MS_RANGE: std::ops::RangeInclusive<u32> = 10..=10_000;
/// Longest token, in bytes of UTF-8 (contract section 6.2); the shortest is 1 byte.
pub const MAX_TOKEN_BYTES: usize = 256;
/// Longest endpoint file, in bytes (contract section 6.2).
pub const MAX_ENDPOINT_FILE: usize = 4096;
/// The `bye` reason of a host that refuses a `hello` without the right token (contract section 6.2),
/// and the reason of the [`Event::HandshakeFailed`] it reports.
pub const UNAUTHORIZED: &str = "unauthorized";
/// Collision datagrams one [`Link::poll`] sends at most (section 9.5).
pub const COLLISION_PER_POLL: usize = 32;
/// The UDP receive buffer a receiver asks for, in bytes (section 2); the OS may grant less.
pub const RECV_BUFFER_BYTES: usize = 4 << 20;
/// How long one connection attempt of a guest may take. It runs on a helper thread, so a slow
/// refusal (Windows retries a refused localhost SYN for about two seconds) never blocks a poll.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a guest on a target with no threads (`wasm32-wasip2`) waits inside [`Link::poll`] for its
/// connection. A poll is one tick of the caller's loop (50 ms at 20 ticks a second), a loopback connect to a
/// listener with room in its accept queue takes well under a millisecond (0.2 to 0.7 ms in 20 attempts under
/// wasmtime), and a host that never accepts
/// (a full accept queue drops the SYN, and the system retries it for seconds) must not hold more than two
/// ticks of that loop, repeated after every [`LinkConfig::reconnect`].
const INLINE_CONNECT_TIMEOUT: Duration = Duration::from_millis(100);

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

/// `(a, b)` such that a perspective projection's clip depth is `z_clip = a * z_view + b` while
/// `w_clip = -z_view`, for the three conventions of [`DepthRange`] (contract section 9.3).
fn depth_terms(near: f32, far: f32, range: DepthRange) -> (f32, f32) {
    let (n, f) = (f64::from(near), f64::from(far));
    let (a, b) = match (range, far.is_finite()) {
        (DepthRange::NegOneToOne, true) => ((f + n) / (n - f), 2.0 * f * n / (n - f)),
        (DepthRange::NegOneToOne, false) => (-1.0, -2.0 * n),
        (DepthRange::ZeroToOne, true) => (f / (n - f), f * n / (n - f)),
        (DepthRange::ZeroToOne, false) => (-1.0, -n),
        (DepthRange::Reversed, true) => (n / (f - n), n * f / (f - n)),
        (DepthRange::Reversed, false) => (0.0, n),
    };
    (a as f32, b as f32)
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
        let (a, b) = depth_terms(near, far, DepthRange::NegOneToOne);
        proj[10] = a;
        proj[14] = b;
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

    /// The same camera with its projection's depth row rewritten for `range` (and
    /// [`Camera::depth_range`] set to match). [`look_at`](Self::look_at) builds an OpenGL range
    /// one; a host whose engine uses another convention converts with this.
    pub fn with_depth_range(mut self, range: DepthRange) -> Self {
        let (a, b) = depth_terms(self.near, self.far, range);
        self.proj[10] = a;
        self.proj[14] = b;
        self.depth_range = range;
        self
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
        if n > usize::from(MAX_COLLISION_CHUNKS) {
            return Err(WireError::BadValue(
                "too many triangles: a snapshot has at most 4096 chunks",
            ));
        }
        let count = n as u16;
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
            if c.chunk_count > MAX_COLLISION_CHUNKS {
                return Err(WireError::BadValue(
                    "chunk_count above MAX_COLLISION_CHUNKS",
                ));
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
            if chunk_count > MAX_COLLISION_CHUNKS {
                return Err(WireError::BadValue("chunk_count"));
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

#[derive(Clone, PartialEq, Eq)]
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
    /// Revision 6: the host's token, sent by a guest that has one (contract section 6.2). A host's
    /// `hello` never carries one. `Debug` does not show it.
    pub token: Option<String>,
}

impl fmt::Debug for Hello {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hello")
            .field("role", &self.role)
            .field("game", &self.game)
            .field("protocol", &self.protocol)
            .field("min_protocol", &self.min_protocol)
            .field("udp_port", &self.udp_port)
            .field("heartbeat_ms", &self.heartbeat_ms)
            .field("timeout_ms", &self.timeout_ms)
            .field("session", &self.session)
            .field("token", &self.token.as_ref().map(|_| Redacted))
            .finish()
    }
}

/// Stands for a token in `Debug` output.
struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
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
                    "game": clean(&h.game),
                    "protocol": h.protocol,
                    "min_protocol": h.min_protocol,
                    "udp_port": h.udp_port,
                    "heartbeat_ms": h.heartbeat_ms,
                    "timeout_ms": h.timeout_ms,
                });
                if let Some(s) = h.session {
                    v["session"] = s.into();
                }
                if let Some(t) = &h.token {
                    v["token"] = clean(t).into();
                }
                v
            }
            Self::Spawn(s) => {
                let mut v = json!({
                    "type": "spawn",
                    "entity": s.entity,
                    "provider": clean(&s.provider),
                    "asset": clean(&s.asset),
                });
                if let Some(n) = &s.name {
                    v["name"] = clean(n).into();
                }
                v
            }
            Self::Despawn { entity } => json!({ "type": "despawn", "entity": entity }),
            Self::Input { focus, reason } => {
                let mut v = json!({ "type": "input", "focus": focus.as_str() });
                if let Some(r) = reason {
                    v["reason"] = clean(r).into();
                }
                v
            }
            Self::FrameChannel(c) => json!({
                "type": "frame_channel",
                "path": clean(&c.path),
                "width": c.width,
                "height": c.height,
                "slots": c.slots,
            }),
            Self::Bye { reason } => json!({ "type": "bye", "reason": clean(reason) }),
            Self::Unknown(t) => json!({ "type": clean(t) }),
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
        // serde_json keeps only the last of a duplicate key, so a NUL in an overridden value would
        // never reach the tree below: look at the text. (Everything else the contract's "Syntax of
        // a line" asks for, serde_json checks itself: surrogates, nesting, number syntax.)
        check_line_text(line).map_err(bad)?;
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
                token: f.opt_str("token")?,
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

/// A string as the wire carries it: U+0000 becomes U+FFFD (section 8, "Syntax of a line", 4), since a
/// line with a NUL escape is a protocol error for every receiver.
fn clean(s: &str) -> String {
    s.replace('\0', "\u{fffd}")
}

/// The two rules of a control line that serde_json cannot enforce for us (section 8, "Syntax of a
/// line", 3 and 4), checked on the text before it is parsed: arrays and objects nest at most
/// [`MAX_JSON_NESTING`] deep, and no string holds a `\u0000` escape. It scans the way a lexer would, so
/// that `"\\u0000"` (a backslash and the characters `u0000`) is not mistaken for an escape and a bracket
/// inside a string does not count. Invalid JSON gives whatever; the parser refuses it afterwards.
fn check_line_text(line: &str) -> Result<(), &'static str> {
    let b = line.as_bytes();
    let (mut i, mut in_string, mut depth) = (0, false, 0usize);
    while i < b.len() {
        match b[i] {
            b'"' => in_string = !in_string,
            b'\\' if in_string => {
                if b.get(i + 1) == Some(&b'u') && b.get(i + 2..i + 6) == Some(b"0000") {
                    return Err("U+0000 in a string");
                }
                i += 1; // the escaped character is not a quote or a backslash that counts
            }
            b'{' | b'[' if !in_string => {
                depth += 1;
                if depth > MAX_JSON_NESTING {
                    return Err("nested too deeply");
                }
            }
            b'}' | b']' if !in_string => depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    Ok(())
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

/// Settings for [`Link::host`] and [`Link::guest`]. `Debug` does not show the token.
#[derive(Clone)]
pub struct LinkConfig {
    /// This side's game, sent in `hello`.
    pub game: String,
    /// Host: the TCP port to listen on (0 = any free port, see [`Link::control_addr`]).
    /// Guest: the host's port to connect to.
    pub control_port: u16,
    /// This side's UDP port (0 = any free port); announced to the peer in `hello`.
    pub udp_port: u16,
    /// How often this side sends a `Heartbeat`. A `hello` carries it as `heartbeat_ms`, which a
    /// peer accepts only from 10 ms to 10 s, so [`Link::host`] and [`Link::guest`] clamp it to
    /// [`HEARTBEAT_MS_RANGE`].
    pub heartbeat: Duration,
    /// Watchdog timeout; the effective one is at least 4 peer heartbeats.
    pub timeout: Duration,
    /// Guest: how often to retry connecting while no host answers.
    pub reconnect: Duration,
    /// Revision 6 (contract section 6.2). Host: the token a guest's `hello` must carry; `None` accepts
    /// any guest, unless [`LinkConfig::endpoint_file`] is set, which makes the host generate one. Guest:
    /// the token to send (replaced by the endpoint file's when that is set). 1 to [`MAX_TOKEN_BYTES`]
    /// bytes, no U+0000; [`Link::host`] and [`Link::guest`] refuse anything else.
    pub token: Option<String>,
    /// Revision 6 (contract section 6.2). Host: once the control port is bound, write an [`Endpoint`]
    /// file here with the bound port and the token (generated with [`generate_token`] when
    /// [`LinkConfig::token`] is `None`), and remove it when the link is dropped. Guest: read the port and
    /// the token from this file before every connection attempt, instead of
    /// [`LinkConfig::control_port`] and [`LinkConfig::token`]; while it cannot be read (the host is not
    /// running yet) the guest just tries again after [`LinkConfig::reconnect`].
    pub endpoint_file: Option<PathBuf>,
}

impl fmt::Debug for LinkConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkConfig")
            .field("game", &self.game)
            .field("control_port", &self.control_port)
            .field("udp_port", &self.udp_port)
            .field("heartbeat", &self.heartbeat)
            .field("timeout", &self.timeout)
            .field("reconnect", &self.reconnect)
            .field("token", &self.token.as_ref().map(|_| Redacted))
            .field("endpoint_file", &self.endpoint_file)
            .finish()
    }
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
            token: None,
            endpoint_file: None,
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
    /// Poses of entities the peer spawned (others are dropped and counted in
    /// [`Stats::unknown_entity`]), newer than the last one of their entity.
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
    /// The peer spawned an entity. At most [`MAX_ENTITIES`] are live at once; a spawn beyond that
    /// is ignored (no event, counted in [`Stats::spawn_ignored`]).
    Spawn(Spawn),
    /// The peer despawned an entity it had spawned (a despawn of an unknown id gives no event).
    Despawn {
        entity: u32,
    },
    /// Guest: the host moved the focus. Host: the guest asked for it (grant with
    /// [`Link::set_focus`]).
    Focus {
        focus: Role,
        requested: bool,
    },
    /// The peer announced a pixel file; it replaces the previous one, and the frame numbers
    /// start again at 1 (open it with [`FrameReader::open`]).
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
    /// Pose records for an entity the peer never spawned (or already despawned); not delivered.
    pub unknown_entity: u64,
    /// Spawns ignored because the peer already had [`MAX_ENTITIES`] live entities.
    pub spawn_ignored: u64,
}

struct Ctl {
    stream: Stream,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
}

/// The socket of a control connection, closed so that closing it never waits.
///
/// On `wasm32-wasip2` the runtime takes a write of up to 64 KiB at once and delivers it to the kernel in the
/// background, and closing the socket waits until it has. While the peer neither reads nor closes and the
/// kernel's buffers are full, that is until the peer does: measured with wasmtime 50.0.1, 5 s on macOS and
/// for ever on Linux. A `poll` that closed such a connection, or a `Drop`, would hold its caller that long. A
/// socket that would wait is parked in [`PARKED`] instead and closed by a later [`Link::poll`] once its bytes
/// have gone. Everywhere else the field is closed on drop, as a `TcpStream` always is.
struct Stream(Option<TcpStream>);

impl std::ops::Deref for Stream {
    type Target = TcpStream;
    fn deref(&self) -> &TcpStream {
        self.0.as_ref().expect("a stream is only taken in drop")
    }
}

impl std::ops::DerefMut for Stream {
    fn deref_mut(&mut self) -> &mut TcpStream {
        self.0.as_mut().expect("a stream is only taken in drop")
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let Some(s) = self.0.take() {
            if cfg!(target_os = "wasi") && !closes_at_once(&s) {
                PARKED
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(s);
            }
        }
    }
}

/// Sockets that could not be closed without waiting, see [`Stream`].
static PARKED: Mutex<Vec<TcpStream>> = Mutex::new(Vec::new());

/// Most parked sockets before a `Link` takes no new connection. A host serves one guest at a time, so each
/// parked socket is a session whose peer stopped reading and then stayed; a peer that keeps doing that would
/// otherwise pile up a socket and its kernel buffers (megabytes) per session. A few is already a peer that
/// is not going to read.
const PARKED_LIMIT: usize = 4;

/// Can `s` be closed at once? On `wasm32-wasip2` a write of no bytes asks the runtime whether it still holds
/// bytes for the kernel: it answers `WouldBlock` then, and changes nothing otherwise.
fn closes_at_once(s: &TcpStream) -> bool {
    let mut s = s;
    !matches!(s.write(&[]), Err(e) if e.kind() == ErrorKind::WouldBlock)
}

/// Closes the parked sockets that have nothing left to wait for, and returns how many remain.
fn reap_parked() -> usize {
    let mut parked = PARKED.lock().unwrap_or_else(PoisonError::into_inner);
    parked.retain(|s| !closes_at_once(s));
    parked.len()
}

impl Ctl {
    fn new(stream: TcpStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        stream.set_nodelay(true)?;
        Ok(Self {
            stream: Stream(Some(stream)),
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

    /// Reads what is available and splits complete lines off. The end, if any, applies after the
    /// returned lines: the reason, and whether it is a protocol error (a line over [`MAX_LINE`] or
    /// not UTF-8), which the peer is told about with a `bye` (contract sections 8 and 6.1), unlike a
    /// connection that simply ended.
    fn read_lines(&mut self) -> (Vec<String>, Option<(String, bool)>) {
        let mut lines = Vec::new();
        let mut buf = [0u8; 8192];
        let mut end = None;
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    end = Some(("connection closed".to_owned(), false));
                    break;
                }
                Ok(n) => self.rbuf.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    end = Some((format!("control read failed: {e}"), false));
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
                return (lines, Some(("control line too long".into(), true)));
            }
            match std::str::from_utf8(line) {
                Ok(s) => lines.push(s.to_owned()),
                Err(_) => return (lines, Some(("control line is not UTF-8".into(), true))),
            }
        }
        self.rbuf.drain(..start);
        if self.rbuf.len() >= MAX_LINE {
            return (lines, Some(("control line too long".into(), true)));
        }
        (lines, end)
    }
}

/// A control connection being closed gracefully: its last bytes (usually a `bye`) are flushed,
/// the write side is shut down, and whatever the peer still sends is read and discarded until it
/// closes too or [`LINGER`] passes. Dropping a socket with unread received data makes the OS send
/// a reset instead, and on Windows that reset can destroy the `bye` before the peer reads it.
struct Closing {
    ctl: Ctl,
    deadline: Instant,
    shut: bool,
}

impl Closing {
    fn new(ctl: Ctl, now: Instant) -> Self {
        Self {
            ctl,
            deadline: now + LINGER,
            shut: false,
        }
    }

    /// Makes progress without blocking; false once the socket can be dropped.
    fn pump(&mut self, now: Instant) -> bool {
        if now >= self.deadline || self.ctl.flush().is_err() {
            return false;
        }
        if !self.shut {
            if !self.ctl.wbuf.is_empty() {
                return true;
            }
            if self.ctl.stream.shutdown(Shutdown::Write).is_err() {
                return false;
            }
            self.shut = true;
        }
        let mut buf = [0u8; 4096];
        for _ in 0..64 {
            match self.ctl.stream.read(&mut buf) {
                Ok(0) => return false,
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
        true
    }
}

enum Conn {
    /// No connection. A guest retries at `next_try`; a host waits for one.
    Idle { next_try: Instant },
    /// Guest: a helper thread is connecting to `port`.
    Connecting {
        rx: Receiver<io::Result<TcpStream>>,
        port: u16,
    },
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
/// [`Link::poll`] once per frame or tick; it does not wait on the network, except that a guest on a target with
/// no threads (`wasm32-wasip2`) waits up to 100 ms for its connection.
pub struct Link {
    role: Role,
    cfg: LinkConfig,
    listener: Option<TcpListener>,
    udp: Udp,
    conn: Conn,
    focus: Role,
    seq: u32,
    next_query: u32,
    last_beat: Instant,
    events: Vec<Event>,
    stats: Stats,
    out: Vec<u8>,
    /// Connections being closed gracefully, see [`Closing`].
    closing: Vec<Closing>,
    /// Collision chunks waiting to be sent, [`COLLISION_PER_POLL`] per poll.
    collision_queue: VecDeque<CollisionChunk>,
    // Receive-side filters, reset per session.
    last_camera: Option<u32>,
    /// The peer's live spawned entities, each with the `time_us` of its newest pose.
    entities: HashMap<u32, u64>,
    spawn_ignored_logged: bool,
    last_collision: Option<u32>,
    assembly: Option<Assembly>,
    last_frame: u64,
    /// Host: the token a guest must show. Guest: the token sent with the current connection attempt.
    token: Option<String>,
    /// Host: the endpoint file this link wrote, removed on drop.
    endpoint_written: Option<PathBuf>,
    /// Guest: why the last connection attempt failed, see [`Link::last_error`].
    last_error: Option<String>,
}

fn localhost(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

fn millis(d: Duration) -> u32 {
    d.as_millis().min(u128::from(u32::MAX)) as u32
}

/// The link's UDP socket, bound to 127.0.0.1 and non-blocking.
///
/// Everywhere but `wasm32-wasip2` it is a `std` socket. There the C library's `recvfrom` does not free the
/// payload of the datagram it returns, so every datagram received through `std` leaves its own length (and the
/// allocator's overhead) in the component's memory for good: measured with rustc 1.99.1 and wasmtime 50.0.1,
/// 8208 bytes for one of 8192 and 32 for one of 24, with `recv`, `recv_from` and any buffer size alike, until
/// the memory is exhausted. The same loop through the WASI interface directly stays flat, so this goes through
/// it.
#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
struct Udp(UdpSocket);

#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
impl Udp {
    fn bind(addr: SocketAddr) -> io::Result<Self> {
        let udp = UdpSocket::bind(addr)?;
        request_recv_buffer(&udp, RECV_BUFFER_BYTES);
        udp.set_nonblocking(true)?;
        Ok(Self(udp))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }

    fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.0.recv_from(buf)
    }

    fn send_to(&self, data: &[u8], addr: SocketAddr) -> io::Result<usize> {
        self.0.send_to(data, addr)
    }
}

/// Asks the OS for a UDP receive buffer of `bytes` (contract section 2: collision snapshots
/// arrive in bursts). Best effort: the OS may grant less, or refuse, and the link works either way.
#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
fn request_recv_buffer(udp: &UdpSocket, bytes: usize) {
    let bytes = i32::try_from(bytes).unwrap_or(i32::MAX);
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: a valid socket descriptor, and a pointer to a c_int that lives through the call.
        unsafe {
            libc::setsockopt(
                udp.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                std::ptr::addr_of!(bytes).cast(),
                std::mem::size_of::<i32>() as libc::socklen_t,
            );
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        extern "system" {
            fn setsockopt(s: usize, level: i32, name: i32, value: *const u8, len: i32) -> i32;
        }
        const SOL_SOCKET: i32 = 0xffff;
        const SO_RCVBUF: i32 = 0x1002;
        // SAFETY: a valid socket, and a pointer to an i32 that lives through the call.
        unsafe {
            setsockopt(
                udp.as_raw_socket() as usize,
                SOL_SOCKET,
                SO_RCVBUF,
                std::ptr::addr_of!(bytes).cast(),
                std::mem::size_of::<i32>() as i32,
            );
        }
    }
    #[cfg(not(any(unix, windows)))]
    let _ = (udp, bytes);
}

#[cfg(all(target_os = "wasi", target_env = "p2"))]
use wasip2::sockets::{
    instance_network::instance_network,
    network::{ErrorCode, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress},
    udp::{IncomingDatagramStream, OutgoingDatagram, OutgoingDatagramStream, UdpSocket as WasiUdp},
    udp_create_socket::create_udp_socket,
};

#[cfg(all(target_os = "wasi", target_env = "p2"))]
struct Udp {
    // Fields drop in this order, and the streams must go before the socket they came from.
    incoming: IncomingDatagramStream,
    outgoing: OutgoingDatagramStream,
    _socket: WasiUdp,
    local: SocketAddr,
}

#[cfg(all(target_os = "wasi", target_env = "p2"))]
impl Udp {
    fn bind(addr: SocketAddr) -> io::Result<Self> {
        let SocketAddr::V4(v4) = addr else {
            return Err(ErrorKind::Unsupported.into());
        };
        let net = instance_network();
        let socket = create_udp_socket(IpAddressFamily::Ipv4).map_err(wasi_error)?;
        socket.start_bind(&net, wasi_addr(v4)).map_err(wasi_error)?;
        loop {
            match socket.finish_bind() {
                Ok(()) => break,
                Err(ErrorCode::WouldBlock) => socket.subscribe().block(),
                Err(e) => return Err(wasi_error(e)),
            }
        }
        // A receive buffer of RECV_BUFFER_BYTES (section 2); best effort, the host may grant less.
        let _ = socket.set_receive_buffer_size(RECV_BUFFER_BYTES as u64);
        let local = match socket.local_address().map_err(wasi_error)? {
            IpSocketAddress::Ipv4(a) => SocketAddr::from((
                Ipv4Addr::new(a.address.0, a.address.1, a.address.2, a.address.3),
                a.port,
            )),
            IpSocketAddress::Ipv6(_) => return Err(ErrorKind::Unsupported.into()),
        };
        let (incoming, outgoing) = socket.stream(None).map_err(wasi_error)?;
        Ok(Self {
            incoming,
            outgoing,
            _socket: socket,
            local,
        })
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }

    /// One datagram, or `WouldBlock`. Like the OS, it cuts a datagram that does not fit `buf` and says so
    /// only by the length it returns.
    fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let mut received = self.incoming.receive(1).map_err(wasi_error)?;
        let Some(d) = received.pop() else {
            return Err(ErrorKind::WouldBlock.into());
        };
        let n = d.data.len().min(buf.len());
        buf[..n].copy_from_slice(&d.data[..n]);
        let from = match d.remote_address {
            IpSocketAddress::Ipv4(a) => SocketAddr::from((
                Ipv4Addr::new(a.address.0, a.address.1, a.address.2, a.address.3),
                a.port,
            )),
            // never sent to an IPv4 socket; a source that matches no peer
            IpSocketAddress::Ipv6(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        };
        Ok((n, from))
    }

    fn send_to(&self, data: &[u8], addr: SocketAddr) -> io::Result<usize> {
        let SocketAddr::V4(v4) = addr else {
            return Err(ErrorKind::Unsupported.into());
        };
        if self.outgoing.check_send().map_err(wasi_error)? == 0 {
            return Err(ErrorKind::WouldBlock.into());
        }
        let datagram = OutgoingDatagram {
            data: data.to_vec(),
            remote_address: Some(wasi_addr(v4)),
        };
        match self.outgoing.send(&[datagram]).map_err(wasi_error)? {
            0 => Err(ErrorKind::WouldBlock.into()),
            _ => Ok(data.len()),
        }
    }
}

#[cfg(all(target_os = "wasi", target_env = "p2"))]
fn wasi_addr(a: std::net::SocketAddrV4) -> IpSocketAddress {
    IpSocketAddress::Ipv4(Ipv4SocketAddress {
        port: a.port(),
        address: {
            let [a, b, c, d] = a.ip().octets();
            (a, b, c, d)
        },
    })
}

/// The kinds of error the link tells apart (would block, refused, reset); anything else keeps its WASI name.
#[cfg(all(target_os = "wasi", target_env = "p2"))]
fn wasi_error(e: ErrorCode) -> io::Error {
    let kind = match e {
        ErrorCode::WouldBlock => ErrorKind::WouldBlock,
        ErrorCode::ConnectionRefused => ErrorKind::ConnectionRefused,
        ErrorCode::ConnectionReset => ErrorKind::ConnectionReset,
        ErrorCode::AddressInUse => ErrorKind::AddrInUse,
        ErrorCode::AccessDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Other,
    };
    io::Error::new(kind, format!("{e:?}"))
}

/// A fresh random session number (contract section 6, step 3), from the operating system's generator, like a
/// token. If the generator fails there is no number: the handshake fails with a `bye` rather than go on with a
/// weaker one. The number is what binds the datagrams to the session (section 6.2), `generate_token` makes the
/// same choice, and a generator that fails is a machine the link should not be trusted on. (The hash of
/// `RandomState`, which was the fallback, may be deterministic on WASI: `wasi:random/insecure-seed` says so.)
fn random_session() -> Option<u32> {
    let mut b = [0u8; 4];
    os_random(&mut b).ok()?;
    Some(u32::from_le_bytes(b).max(1))
}

impl Link {
    /// Listens on `127.0.0.1:control_port` and waits, in [`Link::poll`], for one guest at a time.
    ///
    /// With [`LinkConfig::token`] the host refuses every guest whose `hello` does not carry that token,
    /// with `bye` [`UNAUTHORIZED`]. With [`LinkConfig::endpoint_file`] it writes the endpoint file once
    /// its sockets are bound (generating a token if it has none) and fails if it cannot.
    pub fn host(cfg: LinkConfig) -> io::Result<Self> {
        if let Some(t) = &cfg.token {
            check_token(t)?;
        }
        let listener = TcpListener::bind(localhost(cfg.control_port))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let mut link = Self::new(Role::Host, cfg, Some(listener))?;
        link.token = link.cfg.token.clone();
        if let Some(path) = link.cfg.endpoint_file.clone() {
            // Only now, with the port bound: a process squatting the port makes the bind above fail,
            // so it never learns the token from a guest that reads this file (contract section 6.2).
            let token = match link.token.take() {
                Some(t) => t,
                None => generate_token()?,
            };
            let endpoint = Endpoint {
                control_port: port,
                token,
            };
            endpoint.write(&path)?;
            link.token = Some(endpoint.token);
            link.endpoint_written = Some(path);
        }
        Ok(link)
    }

    /// Connects, in [`Link::poll`], to a host on `127.0.0.1:control_port`, retrying until one
    /// answers and reconnecting after a session ends. It sends [`LinkConfig::token`] in its `hello`,
    /// or, with [`LinkConfig::endpoint_file`], the port and token it reads there before each attempt.
    pub fn guest(cfg: LinkConfig) -> io::Result<Self> {
        if let Some(t) = &cfg.token {
            check_token(t)?;
        }
        Self::new(Role::Guest, cfg, None)
    }

    fn new(role: Role, mut cfg: LinkConfig, listener: Option<TcpListener>) -> io::Result<Self> {
        // The `hello` carries `heartbeat_ms`, which a peer accepts only within HEARTBEAT_MS_RANGE.
        let (lo, hi) = (HEARTBEAT_MS_RANGE.start(), HEARTBEAT_MS_RANGE.end());
        cfg.heartbeat = cfg.heartbeat.clamp(
            Duration::from_millis(u64::from(*lo)),
            Duration::from_millis(u64::from(*hi)),
        );
        let udp = Udp::bind(localhost(cfg.udp_port))?;
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
            closing: Vec::new(),
            collision_queue: VecDeque::new(),
            last_camera: None,
            entities: HashMap::new(),
            spawn_ignored_logged: false,
            last_collision: None,
            assembly: None,
            last_frame: 0,
            token: None,
            endpoint_written: None,
            last_error: None,
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

    /// Guest: why the last attempt to connect failed, or `None` before the first attempt and after one that
    /// connected. Either the endpoint file could not be read or was refused, or the connection to the host's
    /// port failed. The text never holds a token. A host that refuses the guest after the connection is up
    /// is reported by [`Event::HandshakeFailed`] instead.
    ///
    /// Either role, on `wasm32-wasip2` only: why it is taking no new connection, while four connections whose
    /// peers stopped reading are still set aside. The four are counted for the whole component, not per `Link`:
    /// unrelated `Link`s in it share them, and one can be held up by another's peers. A host has no other
    /// reason, and clears it when it is under the limit again. Elsewhere there is no such limit.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Which side currently has input focus.
    pub fn focus(&self) -> Role {
        self.focus
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Does all pending network work without waiting for the peer and returns what happened since the last
    /// call. Call it every frame; heartbeats are sent from here, so a side that stops polling
    /// looks dead to its peer, as intended. The one wait: a guest on a target with no threads
    /// (`wasm32-wasip2`) connects here and waits up to 100 ms for a host that does not accept.
    pub fn poll(&mut self) -> Vec<Event> {
        let now = Instant::now();
        self.closing.retain_mut(|c| c.pump(now));
        let parked = if cfg!(target_os = "wasi") {
            reap_parked()
        } else {
            0
        };
        self.poll_connect(now, parked >= PARKED_LIMIT);
        self.poll_control(now);
        self.poll_udp(now);
        self.pump_collision();
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
            // Only a guest's hello carries a token; a host never sends its own (contract section 6.2).
            token: match self.role {
                Role::Guest => self.token.clone(),
                Role::Host => None,
            },
        })
    }

    fn poll_connect(&mut self, now: Instant, full: bool) {
        if let Some(listener) = &self.listener {
            let mut busy = Vec::new();
            // Said while it lasts, and gone when the host is under the limit again.
            self.last_error = full.then(|| {
                "not accepting: peers that stopped reading still hold earlier connections".into()
            });
            loop {
                // With [`PARKED_LIMIT`] sockets parked nobody is taken in; a guest waits in the backlog.
                if full {
                    break;
                }
                let stream = match listener.accept() {
                    Ok((s, _)) => s,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => break,
                };
                let Ok(ctl) = Ctl::new(stream) else { continue };
                if matches!(self.conn, Conn::Idle { .. }) {
                    self.conn = Conn::Handshake { ctl, since: now };
                } else {
                    busy.push(ctl);
                }
            }
            for ctl in busy {
                self.retire(ctl, Some("busy".into()), now);
            }
            return;
        }
        match &self.conn {
            Conn::Idle { next_try } if now >= *next_try && full => {
                self.last_error = Some(
                    "not connecting: peers that stopped reading still hold earlier connections"
                        .into(),
                );
                self.conn = Conn::Idle {
                    next_try: now + self.cfg.reconnect,
                };
            }
            Conn::Idle { next_try } if now >= *next_try => {
                let port = match &self.cfg.endpoint_file {
                    // Read before every attempt: a restarted host has a new port and a new token.
                    Some(path) => match Endpoint::read(path) {
                        Ok(e) => {
                            self.token = Some(e.token);
                            e.control_port
                        }
                        Err(e) => {
                            // `Endpoint::read` never quotes the file, so this holds no token. Its own
                            // refusals already begin with "endpoint file: ".
                            let why = e.to_string();
                            self.last_error = Some(if why.starts_with("endpoint file: ") {
                                why
                            } else {
                                format!("endpoint file: {why}")
                            });
                            self.conn = Conn::Idle {
                                next_try: now + self.cfg.reconnect,
                            };
                            return;
                        }
                    },
                    None => {
                        self.token = self.cfg.token.clone();
                        self.cfg.control_port
                    }
                };
                let addr = localhost(port);
                // A target with no threads (`wasm32-wasip2`) connects right here, for at most
                // `INLINE_CONNECT_TIMEOUT`; the helper thread below does not exist there.
                if cfg!(target_os = "wasi") {
                    let connected = TcpStream::connect_timeout(&addr, INLINE_CONNECT_TIMEOUT)
                        // On `wasm32-wasip2` this returns `Ok` for a port that refuses; only the socket's own
                        // error tells.
                        .and_then(|s| match s.take_error() {
                            Ok(None) => Ok(s),
                            Ok(Some(e)) | Err(e) => Err(e),
                        })
                        .and_then(Ctl::new);
                    self.conn = self.connect_done(connected, port, now);
                    return;
                }
                let (tx, rx) = mpsc::channel();
                let spawned = std::thread::Builder::new()
                    .name("rewrite-bridge-connect".into())
                    .spawn(move || {
                        let _ = tx.send(TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT));
                    });
                self.conn = match spawned {
                    Ok(_) => Conn::Connecting { rx, port },
                    Err(e) => self.connect_done(Err(e), port, now),
                };
            }
            Conn::Connecting { rx, port } => {
                let port = *port;
                let connected = match rx.try_recv() {
                    Err(TryRecvError::Empty) => return,
                    Ok(r) => r.and_then(Ctl::new),
                    Err(TryRecvError::Disconnected) => Err(ErrorKind::BrokenPipe.into()),
                };
                self.conn = self.connect_done(connected, port, now);
            }
            _ => {}
        }
    }

    /// What follows an attempt to connect to `port`: our `hello` and the handshake, or, after recording why
    /// it failed (the kind of the error only, which quotes nothing), a pause before the next try.
    fn connect_done(&mut self, connected: io::Result<Ctl>, port: u16, now: Instant) -> Conn {
        match connected {
            Ok(mut ctl) => {
                self.last_error = None;
                ctl.queue(&self.hello(PROTOCOL_VERSION, None));
                Conn::Handshake { ctl, since: now }
            }
            Err(e) => {
                self.last_error = Some(format!("cannot connect to 127.0.0.1:{port}: {}", e.kind()));
                Conn::Idle {
                    next_try: now + self.cfg.reconnect,
                }
            }
        }
    }

    /// Closes a control connection gracefully, after an optional `bye`.
    fn retire(&mut self, mut ctl: Ctl, bye: Option<String>, now: Instant) {
        if let Some(reason) = bye {
            ctl.queue(&Control::Bye { reason });
            self.stats.lines_sent += 1;
        }
        let mut c = Closing::new(ctl, now);
        if c.pump(now) {
            self.closing.push(c);
        }
    }

    fn ctl(&mut self) -> Option<&mut Ctl> {
        match &mut self.conn {
            Conn::Handshake { ctl, .. } | Conn::Live { ctl, .. } => Some(ctl),
            Conn::Idle { .. } | Conn::Connecting { .. } => None,
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
        if let Some((reason, bye)) = end {
            self.end(reason, bye);
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
            Control::Spawn(s) => {
                // A second spawn of a live id replaces the first; a new id needs a free place.
                if !self.entities.contains_key(&s.entity) {
                    if self.entities.len() >= MAX_ENTITIES {
                        self.stats.spawn_ignored += 1;
                        if !std::mem::replace(&mut self.spawn_ignored_logged, true) {
                            eprintln!(
                                "rewrite-bridge: the peer has {MAX_ENTITIES} live entities; \
                                 further spawns are ignored (entity {} and later ones)",
                                s.entity
                            );
                        }
                        return Ok(());
                    }
                    self.entities.insert(s.entity, 0);
                }
                self.events.push(Event::Spawn(s));
            }
            Control::Despawn { entity } => {
                if self.entities.remove(&entity).is_some() {
                    self.events.push(Event::Despawn { entity });
                }
            }
            Control::Input { focus, .. } => {
                let requested = self.role == Role::Host;
                if !requested {
                    self.focus = focus;
                }
                self.events.push(Event::Focus { focus, requested });
            }
            Control::FrameChannel(c) => {
                // A new pixel file numbers its frames from 1 again (contract section 8).
                self.last_frame = 0;
                self.events.push(Event::FrameChannel(c));
            }
            Control::Bye { .. } | Control::Unknown(_) => {}
        }
        Ok(())
    }

    fn on_hello(&mut self, h: Hello, now: Instant) -> Result<(), (String, bool)> {
        // The token first, before any check whose answer would tell an unauthenticated peer something
        // (contract section 6.2). The answer never quotes either token.
        if self.role == Role::Host {
            if let Some(want) = &self.token {
                let ok = h.token.as_deref().is_some_and(|got| {
                    (1..=MAX_TOKEN_BYTES).contains(&got.len())
                        && token_equal(got.as_bytes(), want.as_bytes())
                });
                if !ok {
                    return Err((UNAUTHORIZED.to_owned(), true));
                }
            }
        }
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
        if !HEARTBEAT_MS_RANGE.contains(&h.heartbeat_ms) {
            return Err((
                format!(
                    "peer announced heartbeat_ms {}, outside {}..={}",
                    h.heartbeat_ms,
                    HEARTBEAT_MS_RANGE.start(),
                    HEARTBEAT_MS_RANGE.end()
                ),
                true,
            ));
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
                let session = random_session().ok_or_else(|| {
                    ("no source of random bytes for the session".to_owned(), true)
                })?;
                (v, session)
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
        self.entities.clear();
        self.spawn_ignored_logged = false;
        self.collision_queue.clear();
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
        if let Conn::Handshake { ctl, .. } | Conn::Live { ctl, .. } =
            std::mem::replace(&mut self.conn, Conn::Idle { next_try })
        {
            self.retire(ctl, bye.then(|| reason.clone()), now);
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
                let (mut stale, mut unknown) = (0u64, 0u64);
                poses.retain(|p| match self.entities.get_mut(&p.entity) {
                    None => {
                        unknown += 1;
                        false
                    }
                    Some(last) if p.time_us >= *last => {
                        *last = p.time_us;
                        true
                    }
                    Some(_) => {
                        stale += 1;
                        false
                    }
                });
                self.stats.stale += stale;
                self.stats.unknown_entity += unknown;
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

    /// Queues a collision snapshot, in as many chunks as it needs, and returns its query number.
    /// The chunks go out [`COLLISION_PER_POLL`] per [`Link::poll`], so a big snapshot never
    /// overflows the receiver's socket buffer; a snapshot queued while an older one still has
    /// chunks waiting replaces them (the receiver would discard the old query anyway).
    pub fn send_collision(
        &mut self,
        center: [f32; 3],
        radius: f32,
        triangles: &[Triangle],
    ) -> Result<u32, LinkError> {
        if !self.is_connected() {
            return Err(LinkError::NotConnected);
        }
        if triangles.len().div_ceil(MAX_TRIANGLES) > usize::from(MAX_COLLISION_CHUNKS) {
            return Err(WireError::BadValue(
                "too many triangles: a snapshot has at most 4096 chunks",
            )
            .into());
        }
        let c = Collision {
            query: self.next_query,
            center,
            radius,
            triangles: triangles.to_vec(),
        };
        self.collision_queue = c.chunks()?.into();
        self.next_query = self.next_query.wrapping_add(1);
        Ok(c.query)
    }

    /// Sends up to [`COLLISION_PER_POLL`] queued collision chunks.
    fn pump_collision(&mut self) {
        for _ in 0..COLLISION_PER_POLL {
            let Some(chunk) = self.collision_queue.pop_front() else {
                break;
            };
            if self.send(&Datagram::Collision(chunk)).is_err() {
                self.collision_queue.clear();
            }
        }
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
        if self.ctl().is_some() {
            self.end(reason.to_owned(), true);
            self.events.pop(); // The caller knows; don't report its own disconnect back.
        }
    }
}

impl Drop for Link {
    /// Removes the endpoint file the host wrote, says `bye` and closes gracefully, which may block
    /// for up to [`LINGER`]. A connection whose peer has stopped reading is not waited for beyond that, even
    /// on `wasm32-wasip2`: it is set aside and closed later, as the runtime allows, or left to the runtime, which frees it with the component.
    fn drop(&mut self) {
        if let Some(path) = self.endpoint_written.take() {
            let _ = Endpoint::remove(&path);
        }
        let now = Instant::now();
        if let Conn::Handshake { ctl, .. } | Conn::Live { ctl, .. } =
            std::mem::replace(&mut self.conn, Conn::Idle { next_try: now })
        {
            self.retire(ctl, Some("closing".into()), now);
        }
        while !self.closing.is_empty() {
            std::thread::sleep(Duration::from_millis(2));
            let now = Instant::now();
            self.closing.retain_mut(|c| c.pump(now));
        }
        if cfg!(target_os = "wasi") {
            reap_parked();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Token and endpoint file (contract section 6.2)

/// Compares two tokens in constant time (contract section 6.2): the time depends on neither content
/// nor on where they differ. Each is taken as if copied into [`MAX_TOKEN_BYTES`] zero bytes, every byte
/// pair is compared with no early exit, and the lengths are compared too. A token longer than
/// [`MAX_TOKEN_BYTES`] is never equal to anything.
pub fn token_equal(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..MAX_TOKEN_BYTES {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    std::hint::black_box(diff) == 0 && a.len() <= MAX_TOKEN_BYTES
}

/// A new token: 32 bytes from the operating system's cryptographic random number generator, as 64
/// lowercase hex digits (contract section 6.2). Fails rather than fall back to a weaker source.
pub fn generate_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    os_random(&mut bytes)?;
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push(char::from(b"0123456789abcdef"[usize::from(b >> 4)]));
        s.push(char::from(b"0123456789abcdef"[usize::from(b & 15)]));
    }
    Ok(s)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn os_random(buf: &mut [u8]) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let rest = &mut buf[done..];
        // SAFETY: the pointer and length describe `rest`, which lives through the call.
        let n = unsafe { libc::getrandom(rest.as_mut_ptr().cast(), rest.len(), 0) };
        if n > 0 {
            done += n as usize;
            continue;
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) => {}
            // A kernel older than 3.17: the device is the same generator.
            Some(libc::ENOSYS) => return File::open("/dev/urandom")?.read_exact(&mut buf[done..]),
            _ => return Err(e),
        }
    }
    Ok(())
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn os_random(buf: &mut [u8]) -> io::Result<()> {
    // macOS and the BSDs: /dev/urandom is the kernel's CSPRNG (the one arc4random and getentropy use),
    // and never blocks once the system has booted.
    File::open("/dev/urandom")?.read_exact(buf)
}

#[cfg(windows)]
fn os_random(buf: &mut [u8]) -> io::Result<()> {
    #[link(name = "bcrypt")]
    extern "system" {
        fn BCryptGenRandom(alg: *mut u8, buf: *mut u8, len: u32, flags: u32) -> i32;
    }
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 2;
    for chunk in buf.chunks_mut(u32::MAX as usize) {
        // SAFETY: no algorithm handle (the system's preferred generator), and a buffer of `len` bytes.
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                chunk.as_mut_ptr(),
                chunk.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status != 0 {
            return Err(io::Error::other("BCryptGenRandom failed"));
        }
    }
    Ok(())
}

#[cfg(target_os = "wasi")]
fn os_random(buf: &mut [u8]) -> io::Result<()> {
    // WASI `random`, which the runtime fills from the operating system's generator.
    getrandom::fill(buf).map_err(|_| io::Error::other("the WASI random number generator failed"))
}

#[cfg(not(any(unix, windows, target_os = "wasi")))]
fn os_random(_buf: &mut [u8]) -> io::Result<()> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        "no operating system random number generator on this platform",
    ))
}

/// A configured token must be 1 to [`MAX_TOKEN_BYTES`] bytes without U+0000 (which the wire cannot
/// carry). The error does not quote it.
fn check_token(t: &str) -> io::Result<()> {
    if !(1..=MAX_TOKEN_BYTES).contains(&t.len()) || t.contains('\0') {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "token must be 1 to 256 bytes of UTF-8 without U+0000",
        ));
    }
    Ok(())
}

/// The endpoint file of contract section 6.2: the port a host listens on and its token, written by
/// the host after it has bound the port, owner-only, and read by the guest. `Debug` does not show the
/// token.
#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub control_port: u16,
    pub token: String,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("control_port", &self.control_port)
            .field("token", &Redacted)
            .finish()
    }
}

fn endpoint_error(why: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, format!("endpoint file: {why}"))
}

impl Endpoint {
    /// Writes the file atomically: a temporary file in the same directory, created exclusively and
    /// owner-only (mode 0600 on Unix, from the moment it exists), then renamed over `path`. A reader
    /// sees no file, the old one or this one, never part of it. The directory must exist; on Windows
    /// put it under the user's local application data, whose ACL keeps other users out. On WASI there
    /// are no mode bits to ask for, so the file gets the mode of the process's umask (run it with `umask
    /// 077`); that is a deviation the platform forces from section 6.2. Errors never quote the token.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        check_token(&self.token)?;
        if self.control_port == 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "endpoint file: control_port 0",
            ));
        }
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(ErrorKind::InvalidInput, "endpoint file: no file name")
        })?;
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        // The nonce keeps two writers apart (so does `create_new`, which fails rather than share a file);
        // no process id is in the name, since `wasm32-wasip2` has none and `std::process::id` panics there.
        let mut nonce = [0u8; 4];
        os_random(&mut nonce)?;
        let tmp = dir.join(format!(
            ".{}.{:08x}.tmp",
            name.to_string_lossy(),
            u32::from_le_bytes(nonce)
        ));
        let mut text =
            json!({ "control_port": self.control_port, "token": self.token }).to_string();
        text.push('\n');
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        let written = f
            .write_all(text.as_bytes())
            .and_then(|()| f.sync_all())
            .and_then(|()| {
                drop(f);
                std::fs::rename(&tmp, path)
            });
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written
    }

    /// Reads and checks the file (contract section 6.2): a regular file (checked before opening, so a
    /// FIFO cannot block the reader), on Unix with no permission for group or others, at most
    /// [`MAX_ENDPOINT_FILE`] bytes, one JSON object in the syntax of a control line with `control_port`
    /// 1 to 65535 and `token` 1 to [`MAX_TOKEN_BYTES`] bytes. Errors never quote the file.
    pub fn read(path: &Path) -> io::Result<Self> {
        check_endpoint_meta(&std::fs::metadata(path)?)?;
        let mut opts = OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NONBLOCK);
        }
        let f = opts.open(path)?;
        // The opened file is what counts: the path may have been swapped since the check above.
        check_endpoint_meta(&f.metadata()?)?;
        let mut data = Vec::with_capacity(512);
        f.take(MAX_ENDPOINT_FILE as u64 + 1)
            .read_to_end(&mut data)?;
        Self::parse(&data)
    }

    /// Parses the contents of an endpoint file, as [`Endpoint::read`] does after its file checks.
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        if data.len() > MAX_ENDPOINT_FILE {
            return Err(endpoint_error("longer than 4096 bytes"));
        }
        let body = data
            .strip_suffix(b"\r\n")
            .or_else(|| data.strip_suffix(b"\n"))
            .unwrap_or(data);
        if body.contains(&b'\n') {
            return Err(endpoint_error("more than one line"));
        }
        let text = std::str::from_utf8(body).map_err(|_| endpoint_error("not UTF-8"))?;
        check_line_text(text).map_err(endpoint_error)?;
        let v: Value =
            serde_json::from_str(text).map_err(|_| endpoint_error("not a JSON object"))?;
        let o = v
            .as_object()
            .ok_or_else(|| endpoint_error("not a JSON object"))?;
        let f = Fields(o);
        let control_port: u16 = f
            .int("control_port")
            .ok()
            .filter(|&p| p != 0)
            .ok_or_else(|| endpoint_error("control_port is not an integer from 1 to 65535"))?;
        let token = f
            .str("token")
            .ok()
            .filter(|t| (1..=MAX_TOKEN_BYTES).contains(&t.len()))
            .ok_or_else(|| endpoint_error("token is not a string of 1 to 256 bytes"))?
            .to_owned();
        Ok(Self {
            control_port,
            token,
        })
    }

    /// Removes the file (what a host does when it shuts down cleanly).
    pub fn remove(path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
}

fn check_endpoint_meta(m: &std::fs::Metadata) -> io::Result<()> {
    if !m.is_file() {
        return Err(endpoint_error("not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if m.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "endpoint file: group or others have permissions on it",
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Pixel file

const FILE_HEADER_LEN: usize = 64;
const SLOT_HEADER_LEN: usize = 64;
const FRAME_MAGIC: &[u8; 8] = b"RBFRAME\0";
/// `format` value for RGBA8, sRGB, straight alpha, rows top to bottom.
pub const FORMAT_RGBA8_SRGB: u32 = 1;
pub const MAX_SLOTS: u32 = 16;

/// What the depth plane of a pixel file holds (the file header's `depth_format`, revision 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepthFormat {
    /// No depth plane: the file is exactly what revision 2 and earlier wrote.
    None = 0,
    /// One `f32` per pixel: linear view depth in metres, the distance along the camera's forward
    /// axis (positive in front of the camera). `+inf`, or any non-finite value, means "nothing
    /// here".
    LinearMetres = 1,
}

impl DepthFormat {
    /// The format a header value names, `None` for a value this revision does not know.
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::None),
            1 => Some(Self::LinearMetres),
            _ => None,
        }
    }
}

/// Size and shape of the file a [`FrameWriter`] creates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameFileOptions {
    /// Largest frame a slot holds, in pixels.
    pub width: u32,
    pub height: u32,
    /// 1 to [`MAX_SLOTS`]; 3 is recommended.
    pub slots: u32,
    pub depth: DepthFormat,
}

impl FrameFileOptions {
    /// A colour-only file, like [`FrameWriter::create`] makes.
    pub fn new(width: u32, height: u32, slots: u32) -> Self {
        Self {
            width,
            height,
            slots,
            depth: DepthFormat::None,
        }
    }

    /// The same file with a linear-metres depth plane in every slot.
    pub fn with_depth(mut self) -> Self {
        self.depth = DepthFormat::LinearMetres;
        self
    }
}

fn round_up_64(n: u64) -> Option<u64> {
    Some(n.checked_add(63)? / 64 * 64)
}

/// Where a slot's depth plane starts and how big the slot is: `(depth_offset, slot_size)`, or
/// `None` if the arithmetic overflows. `depth_stride == 0` means no depth plane.
fn slot_layout(stride: u32, depth_stride: u32, height: u32) -> Option<(u64, u64)> {
    let colour_end = (u64::from(stride) * u64::from(height)).checked_add(SLOT_HEADER_LEN as u64)?;
    if depth_stride == 0 {
        return Some((0, round_up_64(colour_end)?));
    }
    let depth_offset = round_up_64(colour_end)?;
    let end = depth_offset.checked_add(u64::from(depth_stride) * u64::from(height))?;
    Some((depth_offset, round_up_64(end)?))
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

/// Why [`FrameReader::read`] returned no frame. All of them mean "wait for the next one", except
/// [`FrameMiss::Gone`], after which the reader is of no further use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameMiss {
    /// The slot or size in the datagram does not fit the file.
    OutOfRange,
    /// The writer was in the slot, or overwrote it while it was copied.
    Torn,
    /// The slot already holds another frame.
    Replaced,
    /// The file is now shorter than the mapping made of it (the peer truncated it). Nothing was
    /// read; drop the reader and wait for a new `frame_channel`.
    Gone,
}

/// Fails where the pixel file cannot work: it is memory-mapped, and where the platform has no memory-mapped
/// files (`wasm32-wasip2`) memmap2 is a stub that fails, but only after a writer has made the file.
fn pixel_file_supported() -> io::Result<()> {
    if cfg!(not(any(unix, windows))) {
        return Err(io::Error::new(
            ErrorKind::Unsupported,
            "the pixel file needs memory-mapped files, which this platform does not have",
        ));
    }
    Ok(())
}

/// Writes RGBA frames, and optionally their depth, into a memory-mapped pixel file (contract
/// section 9.6).
pub struct FrameWriter {
    /// Unmapped by hand in `drop`, before the file is deleted: Windows refuses to delete a file
    /// this process still maps.
    map: ManuallyDrop<MmapMut>,
    path: PathBuf,
    channel: FrameChannel,
    stride: u32,
    /// Bytes per depth row, 0 when the file has no depth plane.
    depth_stride: u32,
    depth_offset: u64,
    slot_size: u64,
    next_frame: u64,
}

impl FrameWriter {
    /// Creates a colour-only file at `path` with room for `slots` frames of up to `width` x
    /// `height` pixels. The path must not exist (contract section 9.6: a sender never reuses one).
    pub fn create(path: &Path, width: u32, height: u32, slots: u32) -> io::Result<Self> {
        Self::create_with(path, &FrameFileOptions::new(width, height, slots))
    }

    /// Like [`create`](Self::create), with the options of [`FrameFileOptions`], e.g. a depth plane.
    pub fn create_with(path: &Path, opts: &FrameFileOptions) -> io::Result<Self> {
        pixel_file_supported()?;
        let invalid = |m: &str| io::Error::new(ErrorKind::InvalidInput, m.to_owned());
        let FrameFileOptions {
            width,
            height,
            slots,
            depth,
        } = *opts;
        if !(1..=MAX_SLOTS).contains(&slots) || width == 0 || height == 0 {
            return Err(invalid("slots must be 1 to 16 and the size nonzero"));
        }
        if width > MAX_FRAME_DIM || height > MAX_FRAME_DIM {
            return Err(invalid("width and height are at most 16384"));
        }
        if u64::from(width) * u64::from(height) > MAX_FRAME_PIXELS {
            return Err(invalid("width * height is at most 67108864 pixels"));
        }
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| invalid("width too large"))?;
        let depth_stride = if depth == DepthFormat::None {
            0
        } else {
            stride
        };
        let (depth_offset, slot_size) = slot_layout(stride, depth_stride, height)
            .ok_or_else(|| invalid("the frame size overflows"))?;
        let len = slot_size
            .checked_mul(u64::from(slots))
            .and_then(|n| n.checked_add(FILE_HEADER_LEN as u64))
            .ok_or_else(|| invalid("the file size overflows"))?;
        // `create_new`: an existing file (a peer may still map it) is never truncated or reused.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        if let Err(e) = file.set_len(len) {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
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
        w.u32(depth as u32);
        w.u32(depth_stride);
        w.u64(depth_offset);
        h.resize(FILE_HEADER_LEN, 0);
        map[..FILE_HEADER_LEN].copy_from_slice(&h);
        let path = std::path::absolute(path)?;
        Ok(Self {
            map: ManuallyDrop::new(map),
            channel: FrameChannel {
                path: path.to_string_lossy().into_owned(),
                width,
                height,
                slots,
            },
            path,
            stride,
            depth_stride,
            depth_offset,
            slot_size,
            next_frame: 1,
        })
    }

    /// Creates a new file in the temp directory, named after this process, `tag` and a counter, so
    /// no two calls share a path. (A path a peer may still have mapped cannot be resized or
    /// replaced on Windows; announce a fresh one instead of reusing it.)
    pub fn create_temp(tag: &str, width: u32, height: u32, slots: u32) -> io::Result<Self> {
        Self::create_temp_with(tag, &FrameFileOptions::new(width, height, slots))
    }

    /// [`create_temp`](Self::create_temp) with [`FrameFileOptions`].
    pub fn create_temp_with(tag: &str, opts: &FrameFileOptions) -> io::Result<Self> {
        pixel_file_supported()?;
        static NEXT: AtomicU32 = AtomicU32::new(0);
        // A crashed run may have left a file with this process id's name behind; skip its path.
        for _ in 0..64 {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let name = format!("rewrite-bridge-{}-{tag}-{n}.frames", std::process::id());
            match Self::create_with(&std::env::temp_dir().join(name), opts) {
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                other => return other,
            }
        }
        Err(io::Error::from(ErrorKind::AlreadyExists))
    }

    /// What to pass to [`Link::announce_frames`].
    pub fn channel(&self) -> &FrameChannel {
        &self.channel
    }

    /// Whether every slot has a depth plane, in which case [`write_depth`](Self::write_depth)
    /// must be used.
    pub fn has_depth(&self) -> bool {
        self.depth_stride != 0
    }

    /// Writes one frame of tightly packed RGBA rows into the next slot and returns the
    /// announcement to send with [`Link::send_frame`]. For a file without a depth plane; a file
    /// with one needs [`write_depth`](Self::write_depth), since stale depth from an older frame
    /// would be worse than none.
    pub fn write(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        camera_seq: u32,
        time_us: u64,
    ) -> Result<FrameReady, WireError> {
        self.write_depth(rgba, None, width, height, camera_seq, time_us)
    }

    /// Like [`write`](Self::write), and also the frame's linear depth in metres, `width * height`
    /// values in the same row order as the colour (`+inf` where nothing was drawn). `depth` must be
    /// `Some` exactly when the file has a depth plane.
    pub fn write_depth(
        &mut self,
        rgba: &[u8],
        depth: Option<&[f32]>,
        width: u32,
        height: u32,
        camera_seq: u32,
        time_us: u64,
    ) -> Result<FrameReady, WireError> {
        if width == 0 || height == 0 || width > self.channel.width || height > self.channel.height {
            return Err(WireError::BadValue(
                "a frame is at least 1 x 1 and at most the size of the pixel file",
            ));
        }
        let row = width as usize * 4;
        if rgba.len() != row * height as usize {
            return Err(WireError::BadValue("rgba length is not width * height * 4"));
        }
        match depth {
            Some(_) if !self.has_depth() => {
                return Err(WireError::BadValue("the pixel file has no depth plane"))
            }
            None if self.has_depth() => {
                return Err(WireError::BadValue("the pixel file needs depth"))
            }
            Some(d) if d.len() != width as usize * height as usize => {
                return Err(WireError::BadValue("depth length is not width * height"))
            }
            _ => {}
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
        // SAFETY: every range is inside the slot, which `create_with` sized for the largest frame
        // (colour rows, then the depth plane at `depth_offset`).
        unsafe {
            std::ptr::copy_nonoverlapping(h.as_ptr(), ptr.add(base + 8), h.len());
            for y in 0..height as usize {
                let at = base + SLOT_HEADER_LEN + y * stride;
                std::ptr::copy_nonoverlapping(rgba.as_ptr().add(y * row), ptr.add(at), row);
            }
            if let Some(depth) = depth {
                let dstride = self.depth_stride as usize;
                let plane = base + self.depth_offset as usize;
                for y in 0..height as usize {
                    let dst = ptr.add(plane + y * dstride);
                    for (x, v) in depth[y * width as usize..][..width as usize]
                        .iter()
                        .enumerate()
                    {
                        std::ptr::copy_nonoverlapping(v.to_le_bytes().as_ptr(), dst.add(x * 4), 4);
                    }
                }
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
        // SAFETY: `map` is not used again; unmapping first lets Windows delete the file.
        unsafe { ManuallyDrop::drop(&mut self.map) };
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The slot's seqlock word. The map is page-aligned and every slot starts at a multiple of 64.
fn lock_at<'a, T>(map: *const T, base: usize) -> &'a AtomicU64 {
    // SAFETY: `base` is in bounds and 8-byte aligned (see above); the word is only ever
    // accessed atomically, by this process and the peer.
    unsafe { &*((map as *const u8).add(base) as *const AtomicU64) }
}

/// Opens the file a peer named for reading, if it is an absolute path to a regular file.
///
/// Opening a FIFO for reading blocks until a writer appears, so the type is checked before the
/// open (`symlink_metadata` does not follow a final symlink) and again on the opened handle, and
/// where the OS has `O_NONBLOCK` the open does not wait even if the file is swapped for a FIFO in
/// between. On Unix the handle must be the very file that was checked.
fn open_regular(path: &Path) -> io::Result<File> {
    let bad = |m: &str| io::Error::new(ErrorKind::InvalidInput, m.to_owned());
    if !path.is_absolute() {
        return Err(bad("the pixel file path is not absolute"));
    }
    let before = std::fs::symlink_metadata(path)?;
    if !before.file_type().is_file() {
        return Err(bad("the pixel file is not a regular file"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let after = file.metadata()?;
    if !after.is_file() {
        return Err(bad("the pixel file is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
            return Err(bad("the pixel file was replaced while it was opened"));
        }
    }
    Ok(file)
}

/// The pixel file header's fields after the magic.
struct FileHeader {
    version: u32,
    slots: u32,
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
    slot_size: u64,
    depth_format: u32,
    depth_stride: u32,
    depth_offset: u64,
}

fn parse_file_header(b: &[u8]) -> Result<([u8; 8], FileHeader), WireError> {
    let mut r = In { b, at: 0 };
    Ok((
        r.bytes()?,
        FileHeader {
            version: r.u32()?,
            slots: r.u32()?,
            width: r.u32()?,
            height: r.u32()?,
            stride: r.u32()?,
            format: r.u32()?,
            slot_size: r.u64()?,
            depth_format: r.u32()?,
            depth_stride: r.u32()?,
            depth_offset: r.u64()?,
        },
    ))
}

/// Reads frames from a peer's pixel file, mapped read-only.
///
/// The peer owns the file. A peer that truncates it while it is mapped would crash a reader with
/// SIGBUS on a plain access, so every [`read`](Self::read) first compares the file's current
/// length with the mapped one and answers [`FrameMiss::Gone`] if it shrank. That narrows the race
/// but cannot close it: a truncation between that check and the copy still faults. The contract
/// forbids a sender to truncate or resize a pixel file it announced, so this guards against a
/// broken or hostile peer only.
pub struct FrameReader {
    file: File,
    map: Mmap,
    slots: u32,
    width: u32,
    height: u32,
    stride: u32,
    slot_size: u64,
    /// `(depth_offset, depth_stride)` when the file has a depth plane.
    depth: Option<(u64, u32)>,
}

impl FrameReader {
    /// Maps the file a `frame_channel` message named and checks its header, including the depth
    /// fields: a file whose depth plane does not fit its slot is refused. A `depth_format` this
    /// revision does not know is read as colour only, so a later depth format stays readable.
    ///
    /// The path must be absolute and name a regular file (not a symlink, FIFO, device or
    /// directory), `width` and `height` are at most [`MAX_FRAME_DIM`] with `width * height` at
    /// most [`MAX_FRAME_PIXELS`], and `stride` is exactly
    /// `width * 4` (contract section 9.6). A path that is not a regular file is refused before it
    /// is opened and the file is opened non-blocking where the OS has that, so a FIFO the peer
    /// names cannot stall the caller.
    pub fn open(path: &Path) -> io::Result<Self> {
        pixel_file_supported()?;
        let bad = |m: &str| io::Error::new(ErrorKind::InvalidData, m.to_owned());
        let file = open_regular(path)?;
        let file_len = file.metadata()?.len();
        if file_len < FILE_HEADER_LEN as u64 {
            return Err(bad("pixel file too short"));
        }
        // SAFETY: the file is shared with its writer; this map is only read through raw
        // pointers and the seqlock below, never as a Rust slice that could change under it. A
        // peer that truncates the file is handled in `read_inner` (see the type's docs).
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < FILE_HEADER_LEN {
            return Err(bad("pixel file too short"));
        }
        let (magic, hdr) = parse_file_header(&map[..FILE_HEADER_LEN]).map_err(|_| bad("header"))?;
        let FileHeader {
            version,
            slots,
            width,
            height,
            stride,
            format,
            slot_size: slot_sz,
            ..
        } = hdr;
        if &magic != FRAME_MAGIC || version != 1 || format != FORMAT_RGBA8_SRGB {
            return Err(bad("not a v1 RGBA8 pixel file"));
        }
        if !(1..=MAX_SLOTS).contains(&slots) {
            return Err(bad("inconsistent pixel file header: slots"));
        }
        if !(1..=MAX_FRAME_DIM).contains(&width) || !(1..=MAX_FRAME_DIM).contains(&height) {
            return Err(bad("pixel file width or height outside 1..=16384"));
        }
        if u64::from(width) * u64::from(height) > MAX_FRAME_PIXELS {
            return Err(bad("pixel file width * height above 67108864 pixels"));
        }
        let colour_slot = slot_layout(stride, 0, height).map(|(_, size)| size);
        if u64::from(stride) != u64::from(width) * 4
            || colour_slot.is_none_or(|min| slot_sz < min)
            || slot_sz % 64 != 0
        {
            return Err(bad("inconsistent pixel file header"));
        }
        let depth = match DepthFormat::from_u32(hdr.depth_format) {
            // A later revision's depth format: its colour is still ours to read.
            None | Some(DepthFormat::None) => None,
            Some(DepthFormat::LinearMetres) => {
                let colour_end = SLOT_HEADER_LEN as u64 + u64::from(stride) * u64::from(height);
                let plane = u64::from(hdr.depth_stride) * u64::from(height);
                let fits = hdr
                    .depth_offset
                    .checked_add(plane)
                    .is_some_and(|end| end <= slot_sz);
                if u64::from(hdr.depth_stride) != u64::from(width) * 4
                    || hdr.depth_offset % 64 != 0
                    || hdr.depth_offset < colour_end
                    || !fits
                {
                    return Err(bad("inconsistent depth plane in the pixel file header"));
                }
                Some((hdr.depth_offset, hdr.depth_stride))
            }
        };
        let need = slot_sz
            .checked_mul(u64::from(slots))
            .and_then(|n| n.checked_add(FILE_HEADER_LEN as u64));
        if need.is_none_or(|n| (map.len() as u64) < n) {
            return Err(bad("pixel file shorter than its header says"));
        }
        Ok(Self {
            file,
            map,
            slots,
            width,
            height,
            stride,
            slot_size: slot_sz,
            depth,
        })
    }

    /// Whether the file has a depth plane for [`read_depth`](Self::read_depth) to return.
    pub fn has_depth(&self) -> bool {
        self.depth.is_some()
    }

    /// Copies the announced frame into `rgba` as tightly packed rows, if the slot still holds it
    /// intact. A depth plane, if the file has one, is ignored.
    pub fn read(&self, ready: &FrameReady, rgba: &mut Vec<u8>) -> Result<FrameData, FrameMiss> {
        self.read_inner(ready, rgba, None)
    }

    /// Like [`read`](Self::read), and also copies the frame's linear depth in metres into `depth`
    /// (`width * height` values). A file without a depth plane leaves `depth` empty. The seqlock
    /// covers colour and depth together: either both are from the announced frame or you get a
    /// [`FrameMiss`].
    pub fn read_depth(
        &self,
        ready: &FrameReady,
        rgba: &mut Vec<u8>,
        depth: &mut Vec<f32>,
    ) -> Result<FrameData, FrameMiss> {
        self.read_inner(ready, rgba, Some(depth))
    }

    fn read_inner(
        &self,
        ready: &FrameReady,
        rgba: &mut Vec<u8>,
        depth: Option<&mut Vec<f32>>,
    ) -> Result<FrameData, FrameMiss> {
        if ready.frame == 0
            || ready.slot >= self.slots
            || ready.width == 0
            || ready.height == 0
            || ready.width > self.width
            || ready.height > self.height
        {
            return Err(FrameMiss::OutOfRange);
        }
        // The peer must not shrink its file; if it did, touching the map would fault.
        if self
            .file
            .metadata()
            .map_or(true, |m| m.len() < self.map.len() as u64)
        {
            return Err(FrameMiss::Gone);
        }
        let base = self
            .slot_size
            .checked_mul(u64::from(ready.slot))
            .and_then(|n| n.checked_add(FILE_HEADER_LEN as u64))
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(FrameMiss::OutOfRange)?;
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
        if let Some(depth) = depth {
            depth.clear();
            if let Some((offset, dstride)) = self.depth {
                depth.resize(data.width as usize * data.height as usize, 0.0);
                for y in 0..data.height as usize {
                    let at = base + offset as usize + y * dstride as usize;
                    // SAFETY: as above; `open` checked that the whole plane is inside the slot,
                    // and `row` is at most `dstride`. The destination is the vector's own f32s,
                    // filled bytewise.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            ptr.add(at),
                            (depth.as_mut_ptr() as *mut u8).add(y * row),
                            row,
                        )
                    };
                }
                for v in depth.iter_mut() {
                    *v = f32::from_bits(u32::from_le(v.to_bits()));
                }
            }
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
                token: None,
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
                token: None,
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
        let _ = std::fs::remove_file(&path); // a sender never reuses a path
        let valid = {
            let w = FrameWriter::create(&path, 4, 4, 2).unwrap();
            std::fs::read(w.path()).unwrap()
        };
        std::fs::write(&path, &valid[..100]).unwrap();
        assert!(FrameReader::open(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    // ----- Pixel file with depth (contract section 9.6, revision 3)

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rewrite-bridge-{name}-{}.frames",
            std::process::id()
        ))
    }

    /// Distinct, recognisable pixels and depths for frame `n` of size `w` x `h`.
    fn pattern(n: u8, w: u32, h: u32) -> (Vec<u8>, Vec<f32>) {
        let rgba = (0..w * h * 4)
            .map(|i| (i as u8).wrapping_mul(n).wrapping_add(n))
            .collect();
        let depth = (0..w * h)
            .map(|i| match i % 5 {
                0 => f32::INFINITY,
                1 => f32::NAN,
                _ => f32::from(n) + i as f32 * 0.25,
            })
            .collect();
        (rgba, depth)
    }

    /// Bitwise equality, since the pattern holds NaN.
    fn same_bits(a: &[f32], b: &[f32]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
    }

    #[test]
    fn a_depth_file_has_the_layout_the_contract_gives() {
        let path = temp_path("layout");
        // 8 x 4: colour ends at 64 + 32 * 4 = 192, a multiple of 64; the plane is 128 bytes.
        // 7 x 3: colour ends at 64 + 28 * 3 = 148, so the plane starts at 192 and ends at 276.
        for (w, h, depth_offset, slot_size) in [(8u32, 4u32, 192u64, 320u64), (7, 3, 192, 320)] {
            let wr = FrameWriter::create_with(&path, &FrameFileOptions::new(w, h, 3).with_depth())
                .unwrap();
            assert!(wr.has_depth());
            let b = std::fs::read(&path).unwrap();
            assert_eq!(&b[..8], b"RBFRAME\0");
            assert_eq!(u32_at(&b, 8), 1, "the file version stays 1");
            assert_eq!(u32_at(&b, 12), 3);
            assert_eq!(u32_at(&b, 16), w);
            assert_eq!(u32_at(&b, 20), h);
            assert_eq!(u32_at(&b, 24), w * 4);
            assert_eq!(u32_at(&b, 28), FORMAT_RGBA8_SRGB);
            assert_eq!(u64_at(&b, 32), slot_size);
            assert_eq!(u32_at(&b, 40), 1, "depth_format");
            assert_eq!(u32_at(&b, 44), w * 4, "depth_stride");
            assert_eq!(u64_at(&b, 48), depth_offset);
            assert!(b[56..64].iter().all(|&x| x == 0), "56..64 stay reserved");
            assert_eq!(slot_size % 64, 0);
            assert_eq!(depth_offset % 64, 0);
            assert!(depth_offset >= 64 + u64::from(w * 4 * h));
            assert!(slot_size >= depth_offset + u64::from(w * 4 * h));
            assert_eq!(b.len() as u64, 64 + 3 * slot_size);
            drop(wr);
        }
        // Without depth the same bytes are zero and the slot is the revision-2 size.
        let wr = FrameWriter::create(&path, 7, 3, 3).unwrap();
        assert!(!wr.has_depth());
        let b = std::fs::read(&path).unwrap();
        assert!(b[40..64].iter().all(|&x| x == 0));
        assert_eq!(u64_at(&b, 32), 192);
        drop(wr);
        assert!(!path.exists());
    }

    #[test]
    fn colour_and_depth_round_trip_exactly_through_three_slots() {
        let path = temp_path("depth-roundtrip");
        let mut w =
            FrameWriter::create_with(&path, &FrameFileOptions::new(8, 4, 3).with_depth()).unwrap();
        let r = FrameReader::open(&path).unwrap();
        assert!(r.has_depth());
        let (mut rgba, mut depth) = (Vec::new(), Vec::new());
        // A smaller frame than the file allows is packed tightly, at the file's strides.
        for (n, (fw, fh)) in [
            (1u8, (8, 4)),
            (2, (6, 3)),
            (3, (8, 4)),
            (4, (1, 1)),
            (5, (7, 4)),
        ] {
            let (img, dep) = pattern(n, fw, fh);
            let ready = w
                .write_depth(&img, Some(&dep), fw, fh, u32::from(n) * 10, 99)
                .unwrap();
            assert_eq!(ready.slot, (u64::from(n) % 3) as u32);
            let data = r.read_depth(&ready, &mut rgba, &mut depth).unwrap();
            assert_eq!(rgba, img, "colour of frame {n}");
            assert!(same_bits(&depth, &dep), "depth of frame {n}");
            assert_eq!(
                (data.frame, data.camera_seq),
                (u64::from(n), u32::from(n) * 10)
            );
            assert_eq!((data.width, data.height), (fw, fh));
        }
        // Frame 3 is still in its slot and still reads whole; frame 2's slot was reused by 5.
        let old = FrameReady {
            frame: 3,
            slot: 0,
            camera_seq: 30,
            width: 8,
            height: 4,
        };
        assert_eq!(
            r.read_depth(&old, &mut rgba, &mut depth).map(|d| d.frame),
            Ok(3)
        );
        let gone = FrameReady {
            frame: 2,
            slot: 2,
            camera_seq: 20,
            width: 6,
            height: 3,
        };
        assert_eq!(
            r.read_depth(&gone, &mut rgba, &mut depth),
            Err(FrameMiss::Replaced)
        );
        // The seqlock covers the depth read too: a writer caught mid-frame.
        let (img, dep) = pattern(9, 8, 4);
        let ready = w.write_depth(&img, Some(&dep), 8, 4, 0, 0).unwrap();
        let base = FILE_HEADER_LEN + (w.slot_size * u64::from(ready.slot)) as usize;
        lock_at(w.map.as_mut_ptr(), base).fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            r.read_depth(&ready, &mut rgba, &mut depth),
            Err(FrameMiss::Torn)
        );
        // Wrong arguments are refused instead of leaving stale depth behind.
        assert!(
            w.write(&img, 8, 4, 0, 0).is_err(),
            "a depth file needs depth"
        );
        assert!(w.write_depth(&img, Some(&dep[1..]), 8, 4, 0, 0).is_err());
        drop(r);
        drop(w);
        assert!(!path.exists());
    }

    /// What a revision 2 reader does, written from the old contract text and nothing else: it
    /// knows bytes 0..40 of the file header and a slot's first 36 bytes, finds slots by
    /// `slot_size`, and never looks at the reserved bytes.
    fn rev2_read(file: &[u8], slot: usize) -> (u64, u32, u32, Vec<u8>) {
        assert_eq!(&file[..8], b"RBFRAME\0");
        assert_eq!(u32_at(file, 8), 1);
        let (w, h, stride) = (
            u32_at(file, 16) as usize,
            u32_at(file, 20) as usize,
            u32_at(file, 24) as usize,
        );
        assert_eq!(u32_at(file, 28), 1);
        let slot_size = u64_at(file, 32) as usize;
        assert!(slot_size.is_multiple_of(64) && slot_size >= 64 + stride * h);
        let base = 64 + slot * slot_size;
        let (fw, fh) = (
            u32_at(file, base + 28) as usize,
            u32_at(file, base + 32) as usize,
        );
        assert!(fw <= w && fh <= h);
        let mut px = Vec::new();
        for y in 0..fh {
            px.extend_from_slice(&file[base + 64 + y * stride..][..fw * 4]);
        }
        (u64_at(file, base + 8), fw as u32, fh as u32, px)
    }

    #[test]
    fn v1_readers_still_read_the_colour_of_a_depth_file() {
        let path = temp_path("v1-compat");
        let mut w =
            FrameWriter::create_with(&path, &FrameFileOptions::new(8, 4, 3).with_depth()).unwrap();
        let mut sent = Vec::new();
        for n in 1..=4u8 {
            let (img, dep) = pattern(n, 6, 3);
            sent.push((w.write_depth(&img, Some(&dep), 6, 3, 1, 5).unwrap(), img));
        }
        let file = std::fs::read(&path).unwrap();
        let r = FrameReader::open(&path).unwrap();
        let mut out = Vec::new();
        for (ready, img) in sent.iter().skip(1) {
            // The independent revision 2 reader, on the raw bytes.
            let (frame, fw, fh, px) = rev2_read(&file, ready.slot as usize);
            assert_eq!((frame, fw, fh), (ready.frame, 6, 3));
            assert_eq!(&px, img);
            // And this crate's old entry point, which ignores depth.
            r.read(ready, &mut out).unwrap();
            assert_eq!(&out, img);
        }
        drop(r);
        drop(w);

        // A v1 file (no depth): read_depth works and returns no depth.
        let mut w = FrameWriter::create(&path, 8, 4, 3).unwrap();
        let r = FrameReader::open(&path).unwrap();
        assert!(!r.has_depth());
        let (img, _) = pattern(2, 5, 2);
        let ready = w.write(&img, 5, 2, 7, 1).unwrap();
        let mut depth = vec![1.0];
        let data = r.read_depth(&ready, &mut out, &mut depth).unwrap();
        assert_eq!((out.as_slice(), data.camera_seq), (img.as_slice(), 7));
        assert!(depth.is_empty(), "no depth plane, no depth");
        assert!(w.write_depth(&img, Some(&[0.0; 10]), 5, 2, 0, 0).is_err());
        drop(r);
        drop(w);
        let _ = std::fs::remove_file(&path);
    }

    /// A valid depth file's bytes, to be damaged.
    fn depth_file_bytes(path: &Path) -> Vec<u8> {
        let w =
            FrameWriter::create_with(path, &FrameFileOptions::new(8, 4, 2).with_depth()).unwrap();
        std::fs::read(w.path()).unwrap()
    }

    fn put_u32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u64(b: &mut [u8], at: usize, v: u64) {
        b[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }

    #[test]
    fn an_unknown_depth_format_is_read_as_colour_only() {
        let path = temp_path("future-depth");
        let mut bytes = depth_file_bytes(&path);
        for format in [2, u32::MAX] {
            put_u32(&mut bytes, 40, format);
            std::fs::write(&path, &bytes).unwrap();
            let reader = FrameReader::open(&path).expect("a later depth format still opens");
            assert!(!reader.has_depth(), "format {format}");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn damaged_depth_headers_are_rejected_cleanly() {
        let path = temp_path("bad-depth");
        let good = depth_file_bytes(&path);
        std::fs::write(&path, &good).unwrap();
        assert!(FrameReader::open(&path).is_ok());
        let slot_size = u64_at(&good, 32);
        type Damage = Box<dyn Fn(&mut Vec<u8>)>;
        let cases: Vec<(&str, Damage)> = vec![
            (
                "depth_offset at slot_size",
                Box::new(move |b| put_u64(b, 48, slot_size)),
            ),
            (
                "depth_offset past slot_size",
                Box::new(move |b| put_u64(b, 48, slot_size + 64)),
            ),
            (
                "depth_offset huge",
                Box::new(|b| put_u64(b, 48, u64::MAX - 63)),
            ),
            ("depth_offset overflows", Box::new(|b| put_u64(b, 48, !63))),
            (
                "depth_offset inside the colour",
                Box::new(|b| put_u64(b, 48, 128)),
            ),
            (
                "depth_offset zero with depth",
                Box::new(|b| put_u64(b, 48, 0)),
            ),
            (
                "depth_offset not a multiple of 64",
                Box::new(|b| put_u64(b, 48, 200)),
            ),
            (
                "plane runs past the slot",
                Box::new(|b| put_u64(b, 48, 256)),
            ),
            ("depth_stride too small", Box::new(|b| put_u32(b, 44, 28))),
            ("depth_stride too large", Box::new(|b| put_u32(b, 44, 36))),
            (
                "depth_stride zero with depth",
                Box::new(|b| put_u32(b, 44, 0)),
            ),
            ("depth_stride huge", Box::new(|b| put_u32(b, 44, u32::MAX))),
            (
                "slot_size too small for the plane",
                Box::new(|b| put_u64(b, 32, 256)),
            ),
            (
                "slot_size not a multiple of 64",
                Box::new(|b| put_u64(b, 32, 330)),
            ),
            (
                "file cut inside the last plane",
                Box::new(|b| b.truncate(b.len() - 8)),
            ),
        ];
        for (what, damage) in &cases {
            let mut b = good.clone();
            damage(&mut b);
            std::fs::write(&path, &b).unwrap();
            assert!(FrameReader::open(&path).is_err(), "accepted: {what}");
        }
        // With depth_format 0 the other two fields are not looked at: a colour-only reader of a
        // file from a writer that left bytes there still works.
        let mut b = good.clone();
        put_u32(&mut b, 40, 0);
        put_u64(&mut b, 48, u64::MAX);
        put_u32(&mut b, 44, 12345);
        std::fs::write(&path, &b).unwrap();
        let r = FrameReader::open(&path).unwrap();
        assert!(!r.has_depth());
        drop(r);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fuzzed_pixel_file_headers_never_panic_or_read_out_of_bounds() {
        let path = temp_path("fuzz-depth");
        let good = depth_file_bytes(&path);
        let mut rng = Rng(0xD1CE);
        let ready = |slot| FrameReady {
            frame: 0,
            slot,
            camera_seq: 0,
            width: 8,
            height: 4,
        };
        let (mut opened, mut with_depth) = (0, 0);
        let (mut rgba, mut depth) = (Vec::new(), Vec::new());
        for i in 0..3000 {
            let mut b = good.clone();
            for _ in 0..1 + rng.next() % 4 {
                let at = if i % 2 == 0 {
                    40 + rng.next() as usize % 24 // the new fields
                } else {
                    rng.next() as usize % 64
                };
                match rng.next() % 3 {
                    0 => b[at] = rng.next() as u8,
                    1 => b[at] ^= 1 << (rng.next() % 8),
                    _ => b[at] = [0, 0xFF, 0x40, 0x80][rng.next() as usize % 4],
                }
            }
            std::fs::write(&path, &b).unwrap();
            if let Ok(r) = FrameReader::open(&path) {
                opened += 1;
                with_depth += usize::from(r.has_depth());
                // Whatever opens must be safe to read from, whichever slot is asked for.
                for slot in 0..3 {
                    let _ = r.read_depth(&ready(slot), &mut rgba, &mut depth);
                    let _ = r.read(&ready(slot), &mut rgba);
                }
            }
        }
        assert!(opened > 100 && with_depth > 50, "{opened} {with_depth}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_seqlock_keeps_colour_and_depth_together_under_a_racing_writer() {
        // One slot, so every write lands on the slot the reader is copying.
        let path = temp_path("race");
        let mut w = FrameWriter::create_with(&path, &FrameFileOptions::new(64, 32, 1).with_depth())
            .unwrap();
        let r = FrameReader::open(&path).unwrap();
        let latest = std::sync::Arc::new(AtomicU64::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (l2, s2) = (latest.clone(), stop.clone());
        let writer = std::thread::spawn(move || {
            for n in 1..=3000u64 {
                // Every colour byte and every depth value carries the frame number.
                let img = vec![n as u8; 64 * 32 * 4];
                let dep = vec![n as f32; 64 * 32];
                let ready = w.write_depth(&img, Some(&dep), 64, 32, 0, n).unwrap();
                assert_eq!(ready.frame, n);
                l2.store(n, Ordering::Release);
                std::thread::sleep(Duration::from_micros(30));
            }
            s2.store(true, Ordering::Release);
            w
        });
        let (mut rgba, mut depth) = (Vec::new(), Vec::new());
        let (mut good, mut missed) = (0u32, 0u32);
        while !stop.load(Ordering::Acquire) {
            let n = latest.load(Ordering::Acquire);
            if n == 0 {
                continue;
            }
            let ready = FrameReady {
                frame: n,
                slot: 0,
                camera_seq: 0,
                width: 64,
                height: 32,
            };
            match r.read_depth(&ready, &mut rgba, &mut depth) {
                Ok(data) => {
                    good += 1;
                    assert_eq!(data.frame, n);
                    assert!(
                        rgba.iter().all(|&b| b == n as u8),
                        "torn colour in frame {n}"
                    );
                    assert!(
                        depth.iter().all(|&d| d == n as f32),
                        "torn depth in frame {n}"
                    );
                }
                Err(_) => missed += 1,
            }
        }
        let w = writer.join().unwrap();
        // Once the writer is done the last frame reads whole.
        let last = FrameReady {
            frame: 3000,
            slot: 0,
            camera_seq: 0,
            width: 64,
            height: 32,
        };
        assert!(r.read_depth(&last, &mut rgba, &mut depth).is_ok());
        assert!(good > 0, "{good} whole frames, {missed} misses");
        println!("race test: {good} whole frames, {missed} misses");
        drop(r);
        drop(w);
    }

    // ----- revision 5 (the vectors cover the wire; these cover the pieces the vectors cannot reach)

    #[test]
    fn a_nul_escape_is_refused_wherever_it_is_and_only_there() {
        for line in [
            r#"{"type":"bye","reason":"\u0000"}"#,
            r#"{"type":"bye","reason":"a\u0000b"}"#,
            r#"{"type":"despawn","entity":1,"x":"\u0000"}"#,
            r#"{"type":"despawn","entity":1,"a\u0000b":1}"#,
            r#"{"type":"despawn","entity":1,"x":[{"y":"\u0000"}]}"#,
            // an overridden duplicate is still read
            r#"{"type":"despawn","entity":1,"x":"\u0000","x":1}"#,
            r#"{"type":"bye","reason":"\\\u0000"}"#,
        ] {
            assert!(Control::parse(line).is_err(), "{line}");
        }
        for line in [
            // a backslash followed by the characters u0000 is not an escape
            r#"{"type":"bye","reason":"\\u0000"}"#,
            r#"{"type":"bye","reason":"\\\\u0000"}"#,
            // without the backslash it is just text; other control characters may be escaped
            r#"{"type":"bye","reason":"u0000"}"#,
            r#"{"type":"bye","reason":"\u000a"}"#,
            r#"{"type":"bye","reason":"\u00011"}"#,
        ] {
            assert!(Control::parse(line).is_ok(), "{line}");
        }
    }

    #[test]
    fn nesting_is_limited_to_127_levels_by_this_crate_not_by_serde_json() {
        let line = |n: usize, closed: bool| {
            format!(
                r#"{{"type":"despawn","entity":1,"x":{}{}}}"#,
                "[".repeat(n),
                if closed { "]".repeat(n) } else { String::new() }
            )
        };
        assert!(Control::parse(&line(126, true)).is_ok());
        assert!(Control::parse(&line(127, true)).is_err());
        // an unclosed line is refused by the text scan at the limit, not after a deep recursion
        assert_eq!(
            check_line_text(&line(MAX_JSON_NESTING, false)),
            Err("nested too deeply")
        );
        assert!(check_line_text(&line(MAX_JSON_NESTING - 1, false)).is_ok());
        // brackets inside strings do not count
        let in_string = format!(r#"{{"type":"bye","reason":"{}"}}"#, "[".repeat(500));
        assert!(Control::parse(&in_string).is_ok());
        // nor do escaped quotes end the string early
        let escaped = format!(r#"{{"type":"bye","reason":"\"{}"}}"#, "[".repeat(500));
        assert!(Control::parse(&escaped).is_ok());
    }

    #[test]
    fn a_sender_writes_u_fffd_for_nul() {
        let msgs = [
            Control::Hello(Hello {
                role: Role::Guest,
                game: "a\0b".into(),
                protocol: 1,
                min_protocol: 1,
                udp_port: 9,
                heartbeat_ms: 250,
                timeout_ms: 2000,
                session: None,
                token: None,
            }),
            Control::Spawn(Spawn {
                entity: 1,
                provider: "p\0".into(),
                asset: "\0a".into(),
                name: Some("\0".into()),
            }),
            Control::Input {
                focus: Role::Host,
                reason: Some("x\0y".into()),
            },
            Control::FrameChannel(FrameChannel {
                path: "/tmp/a\0b".into(),
                width: 4,
                height: 2,
                slots: 3,
            }),
            Control::Bye {
                reason: "\0".into(),
            },
            Control::Unknown("t\0".into()),
        ];
        for m in msgs {
            let line = m.to_line();
            assert!(!line.contains('\0') && !line.contains("\\u0000"), "{line}");
            assert!(line.contains('\u{fffd}'), "{line}");
            Control::parse(&line).expect("the line a sender writes is valid");
        }
    }

    #[test]
    fn collision_snapshots_are_limited_to_4096_chunks() {
        assert_eq!(usize::from(MAX_COLLISION_CHUNKS) * MAX_TRIANGLES, 925_696);
        let chunk = |index: u16, count: u16| {
            Datagram::Collision(CollisionChunk {
                query: 1,
                chunk_index: index,
                chunk_count: count,
                center: [0.0; 3],
                radius: 1.0,
                triangles: Vec::new(),
            })
        };
        let mut out = Vec::new();
        assert!(encode_datagram(&header(0), &chunk(4095, MAX_COLLISION_CHUNKS), &mut out).is_ok());
        for count in [4097, 9281, 65535] {
            out.clear();
            assert!(encode_datagram(&header(0), &chunk(0, count), &mut out).is_err());
            // and the decoder drops what a hostile peer writes by hand
            let mut raw = enc(&chunk(0, 1));
            raw[HEADER_LEN + 6..HEADER_LEN + 8].copy_from_slice(&count.to_le_bytes());
            assert_eq!(
                decode_datagram(&raw),
                Err(WireError::BadValue("chunk_count"))
            );
        }
        let limit = usize::from(MAX_COLLISION_CHUNKS) * MAX_TRIANGLES;
        let snapshot = |n: usize| Collision {
            query: 1,
            center: [0.0; 3],
            radius: 1.0,
            triangles: vec![[[0.0; 3]; 3]; n],
        };
        let at_limit = snapshot(limit).chunks().expect("4096 chunks are allowed");
        assert_eq!(at_limit.len(), usize::from(MAX_COLLISION_CHUNKS));
        assert!(at_limit
            .iter()
            .all(|c| c.chunk_count == MAX_COLLISION_CHUNKS));
        assert!(snapshot(limit + 1).chunks().is_err());
    }

    #[test]
    fn a_frame_of_size_zero_is_refused_by_the_writer_and_the_reader() {
        let path = temp_path("size-zero");
        let _ = std::fs::remove_file(&path);
        let mut w = FrameWriter::create(&path, 4, 2, 3).unwrap();
        let (rgba, _) = pattern(1, 4, 2);
        assert!(w.write(&[], 0, 2, 1, 1).is_err());
        assert!(w.write(&[], 4, 0, 1, 1).is_err());
        assert!(w.write(&[], 0, 0, 1, 1).is_err());
        // a refused write used no frame number
        let ready = w.write(&rgba, 4, 2, 7, 9).unwrap();
        assert_eq!((ready.frame, ready.slot), (1, 1));
        let r = FrameReader::open(&path).unwrap();
        let mut buf = Vec::new();
        assert!(r.read(&ready, &mut buf).is_ok());
        for (width, height) in [(0, 2), (4, 0), (0, 0), (3, 2), (4, 1)] {
            let bad = FrameReady {
                width,
                height,
                ..ready
            };
            assert!(r.read(&bad, &mut buf).is_err(), "{width}x{height}");
        }
    }
}
