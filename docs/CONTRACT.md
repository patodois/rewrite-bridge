# Passthrough link contract, protocol v1

This document is the whole protocol. A host plugin written in C, C++, C# or anything else should be
able to implement it from this page alone; the Rust implementation in `src/link.rs` (cargo feature
`link`) is one implementation of it, not the definition.

The link connects two cooperating processes on the same machine: a **host** (the game the player
is in) and a **guest** (a second game or a rewrite that contributes entities, poses or rendered
frames). The link only moves data that each process chooses to publish about itself. It never
reads or writes another process's memory, injects code, or touches anti-cheat.

## 1. Overview

| Channel | Transport | Encoding | Carries |
|---|---|---|---|
| control | TCP, `127.0.0.1` | JSON lines (UTF-8) | `hello`, `spawn`, `despawn`, `input`, `frame_channel`, `bye` |
| state | UDP, `127.0.0.1` | binary, little-endian | `Heartbeat`, `Camera`, `Pose`, `Collision`, `Frame` |
| pixels | memory-mapped file | binary, little-endian | RGBA frames announced by `Frame` |

Who sends what:

| Message | Host to guest | Guest to host |
|---|---|---|
| `hello` | reply, with session | first message |
| `Camera` | yes, every rendered frame | allowed, rarely useful |
| `Collision` | yes, triangles near the player | allowed |
| `Pose` | allowed (host entities) | yes, every tick |
| `spawn` / `despawn` | allowed (host entities) | yes |
| `input` | grants or takes focus | requests focus |
| `frame_channel` / `Frame` | allowed | yes, if the guest renders |
| `Heartbeat` | yes | yes |
| `bye` | yes | yes |

Messages a side does not use may simply be ignored by the receiver.

## 2. Transport

- Everything binds to and connects to `127.0.0.1` only. Implementations must not bind `0.0.0.0`
  or any other interface.
- **Control, TCP.** The host listens on the control port (default **47800**, configurable). The
  guest connects to it. One guest per host: while a guest is connected, the host answers any other
  connection with `bye` (`"reason": "busy"`) and closes it.
- **State, UDP.** Each side binds its own UDP socket on `127.0.0.1` (port configurable; `0` means
  any free port) and announces the port in its `hello`. A side sends datagrams to
  `127.0.0.1:<peer udp_port>` and accepts datagrams only from that exact address. Anything else is
  dropped.
- **Datagram size.** A datagram is at most **8192 bytes** (`MAX_DATAGRAM`). macOS refuses UDP
  datagrams above 9216 bytes by default, and 8192 leaves room. Larger data is chunked (see
  `Collision`) or goes through the pixel file (see `Frame`).
- UDP may drop or reorder datagrams even on loopback when a receive buffer fills. State is
  therefore "latest wins": every datagram is self-contained, and stale ones are discarded (section 7).

## 3. Byte order, layout and numbers

- All binary integers and floats are **little-endian**.
- Floats are IEEE-754 binary32 (`f32`), integers are unsigned unless stated.
- Layouts have **no implicit padding**: every field sits at the offset given below, and every
  field's offset is a multiple of its own size, so the C structs in section 10 have the same layout
  with or without `#pragma pack`. Still, decode by copying (`memcpy`) out of the receive buffer,
  not by casting a pointer into it, since the buffer itself may be unaligned.
- `reserved` fields are written as zero and ignored on read.
- Matrices are 4x4 `f32`, **column-major** (glTF and OpenGL order): element `m[c*4 + r]` is column
  `c`, row `r`; the translation of a transform sits in `m[12..15]`.
- Quaternions are `x, y, z, w`, unit length.

## 4. Coordinates and units

The link uses the same conventions as the crate's `Model` (glTF 2.0). Each side converts its own
native space at the boundary, on send and on receive; the wire is always in this space.

- Units: metres, seconds, radians; velocities in m/s and rad/s.
- Right-handed, **+Y up**. An entity's front faces **+Z**.
- A camera looks down its local **-Z**, with +Y up and +X right (the glTF camera).
- `Camera.view` maps world to view space; `Camera.proj` maps view space to clip space.
- Triangles wind **counter-clockwise** seen from their front (solid side outward).
- Positions, rotations and velocities in `Pose` and `Collision` are in **world space**. "World" is
  the **host's** world converted to these axes: the host owns the shared space, and the guest places
  its content relative to the host's camera and collision.
- Example conversion, a Z-up right-handed game (x forward, y left, z up) into the link space:
  `link = (-y, z, -x)`; the reverse is `native = (-z, -x, y)`. Write such mappings down per game.

## 5. Timestamps and clocks

- Every datagram header carries `time_us`: the sender's **wall clock** when the datagram was
  written, in microseconds since the Unix epoch (UTC), as an unsigned 64-bit integer. Both processes
  run on the same machine and share this clock (`std::chrono::system_clock`,
  `GetSystemTimePreciseAsFileTime`, `clock_gettime(CLOCK_REALTIME)`).
- `Pose.time_us` and the frame slot's `time_us` use the same clock and mark when the sample was
  taken (simulation time), which may be earlier than the send time. Receivers interpolate or
  extrapolate with them.
- Timestamps are for ordering samples and measuring latency. They are **never** used for the
  watchdog, because the wall clock can jump. The watchdog uses each side's own monotonic clock.

## 6. Session lifecycle and version negotiation

1. The host listens on the control port and binds its UDP socket.
2. The guest binds its UDP socket, connects to the control port and sends its `hello` line:
   ```json
   {"type":"hello","role":"guest","game":"Warframe rewrite","protocol":1,"min_protocol":1,
    "udp_port":53001,"heartbeat_ms":250,"timeout_ms":2000}
   ```
3. The host checks it:
   - `role` must be `"guest"`;
   - the version ranges must overlap: the session version is
     `v = min(host.protocol, guest.protocol)`, and it is valid only if
     `v >= max(host.min_protocol, guest.min_protocol)`.

   If either check fails, the host sends `{"type":"bye","reason":"..."}` and closes the
   connection. Otherwise it replies with its own `hello`, with `"protocol"` set to the chosen `v`
   and a fresh nonzero random `session` number:
   ```json
   {"type":"hello","role":"host","game":"War Thunder rewrite","protocol":1,"min_protocol":1,
    "udp_port":53000,"heartbeat_ms":250,"timeout_ms":2000,"session":2882400001}
   ```
4. The guest checks `protocol` is within its own range and `role` is `"host"`; if not, it sends
   `bye` and closes.
5. From then on both sides send UDP datagrams with `version = v` and `session = session`. A
   datagram with another session, another version or another source address is dropped silently.
   The guest sends nothing over UDP before it has the host's `hello`.
6. Either side may end the session with `bye` and close the TCP connection as in section 6.1.
   Closing TCP without `bye` (a crash) ends it too. After the session ends, the host listens for a
   new guest and the guest may reconnect; the new session gets a new `session` number, and
   entities, focus and pixel files from the old one are forgotten.

`hello` has to be the first line in each direction. A side that has not received the peer's
`hello` within its own `timeout_ms` of the TCP connection closes it.

### 6.1 Closing: after sending `bye`, half-close and drain

Every side that closes a control connection, host or guest, for any reason (a rejected `hello`,
`"busy"`, a protocol error, the watchdog, or shutting down), does this:

1. Write the `bye` line and flush it.
2. Half-close: shut down the write side only (`shutdown(fd, SHUT_WR)`, or `SD_SEND` on Windows).
3. Read and discard whatever still arrives, until the peer's end of stream (a read of 0 bytes),
   an error, or **200 ms**, whichever comes first.
4. Close the socket.

The reason: closing a socket that still holds unread received data makes the OS send a reset
instead of a normal close. Windows then discards data the peer has received but not yet read,
so the `bye` is lost and the peer only sees "connection reset" (`WSAECONNRESET`, 10054). Draining
first means the socket closes with nothing unread.

On the receiving side, **a received `bye` decides how the session ended.** Process every complete
line read before a read error; if one of them is a `bye`, a reset or end of stream that follows it
is the clean end of the session, not a failure. A side that receives a `bye` closes its socket as
above, without sending a `bye` of its own.

A game loop must not block for step 3: keep the closing socket in a list and drain it,
non-blocking, on each tick until it is done.

Future versions keep the datagram header's first 8 bytes (magic, version, kind, flags) and the
`hello` fields above, so two implementations can always negotiate.

## 7. Watchdog, heartbeat and late or lost data

- Each side sends a `Heartbeat` datagram every `heartbeat_ms` (its own value from its `hello`;
  default 250 ms), whether or not it sent other datagrams. Sending heartbeats only from the game's
  frame or tick loop is intended: a frozen game stops beating and its peer notices.
- Each side keeps `last_heard`: the monotonic time of the last valid datagram or control line from
  the peer. When `now - last_heard > timeout`, the peer is **lost**. The timeout is
  `max(own timeout_ms, 4 * peer heartbeat_ms)`; the default is 2000 ms.
- A TCP read of 0 bytes (EOF) or a connection error means the peer is lost immediately.
- When the peer is lost, the session ends as in section 6. The host takes back input focus, hides
  or freezes the guest's entities as it sees fit, and stops reading the pixel file.
- **Late or out-of-order UDP**, compared with serial-number arithmetic on 32-bit `seq` (RFC 1982:
  `a` is newer than `b` when `(int32_t)(a - b) > 0`):
  - `Camera`: drop it if its header `seq` is not newer than the newest camera received.
  - `Pose`: per entity, drop a record whose `time_us` is older than the last one used.
  - `Collision`: chunks belong to a `query`; a chunk of an older query than the newest seen is
    dropped, and an incomplete query is discarded when a newer one starts.
  - `Frame`: drop it unless its `frame` number is newer than the newest one shown.
- **Full buffers.** UDP sends that would block are dropped (the next tick replaces them). The
  control channel is reliable; a side whose unsent control bytes exceed 1 MiB treats the peer as
  lost, rather than stall its game.

## 8. Control messages (TCP JSON lines)

- One JSON object per line, UTF-8, ending in `\n` (a preceding `\r` is tolerated). No line may
  exceed **65536 bytes** including the newline; a longer line is a protocol error and ends the
  session.
- Every object has a string field `"type"`. Unknown types and unknown fields are ignored, so later
  versions can add them.
- A line that is not a JSON object with a string `"type"`, or a known type with a missing or
  out-of-range field, is a protocol error: the receiver sends `bye` with the reason and closes.
  (TCP is reliable, so a garbled line means a broken peer, not a lost packet.)
- Entity ids are integers in `0 ..= 4294967295` (they are `u32` in the binary layouts). Each side
  owns its own entity ids: the guest's entity 7 and the host's entity 7 are different entities.

### `hello`

| Field | Type | Meaning |
|---|---|---|
| `role` | `"host"` / `"guest"` | sender's role |
| `game` | string | sender's game, e.g. a `Provider::game()` name |
| `protocol` | integer | highest supported version (in the host reply: the chosen version) |
| `min_protocol` | integer | lowest supported version |
| `udp_port` | integer | the sender's UDP port on 127.0.0.1 |
| `heartbeat_ms` | integer | how often the sender sends `Heartbeat` |
| `timeout_ms` | integer | the sender's own watchdog timeout |
| `session` | integer, host only | nonzero `u32` session number |

### `spawn`

Announces an entity of the sender's, to be drawn with a provider's asset. Its pose arrives in
`Pose` datagrams.

```json
{"type":"spawn","entity":7,"provider":"Warframe","asset":"/Lotus/Characters/Excalibur","name":"Excalibur"}
```

| Field | Type | Meaning |
|---|---|---|
| `entity` | integer | sender's entity id |
| `provider` | string | the asset's game, as `Provider::game()` returns it |
| `asset` | string | the asset id, as `Provider::list` returns it in `AssetInfo.id` |
| `name` | string, optional | display name |

A second `spawn` with the same `entity` replaces the first.

### `despawn`

`{"type":"despawn","entity":7}` removes one of the sender's entities.

### `input`

Says which side receives the player's keyboard, mouse and gamepad. The **host** is the authority
and has the focus when a session starts.

- `{"type":"input","focus":"guest"}` from the host gives the focus to the guest;
  `{"type":"input","focus":"host"}` from the host takes it back. The guest obeys.
- The same message from the guest is a **request**; it takes effect only when the host answers
  with an `input` message of its own. The host may ignore it.
- When the peer is lost, focus returns to the host.

Optional field `"reason"`: a string for logs.

### `frame_channel`

Announces a pixel file (section 9.6) the sender will fill and refer to in `Frame` datagrams.

```json
{"type":"frame_channel","path":"/tmp/rewrite-bridge-2882400001-guest.frames","width":1280,"height":720,"slots":3}
```

`path` is an absolute path to a file the receiver may map read-only. A later `frame_channel`
replaces the previous one (for example after a resize).

### `bye`

`{"type":"bye","reason":"shutting down"}`. The sender then half-closes, drains and closes the
connection (section 6.1). The host also answers a connection it cannot take with
`{"type":"bye","reason":"busy"}`, before or without reading that client's `hello`.

## 9. Datagrams (UDP, binary)

Every datagram is a 24-byte header followed by the payload of its `kind`. A receiver must check
the length before reading any field: a datagram shorter than the header or than its kind's
payload, or whose length does not match the counts it contains, is malformed and dropped.

### 9.1 Header (24 bytes)

| Offset | Size | Type | Field | Value |
|---|---|---|---|---|
| 0 | 4 | u32 | `magic` | `0x4B4C4252` (bytes `R B L K`) |
| 4 | 2 | u16 | `version` | negotiated version, `1` |
| 6 | 1 | u8 | `kind` | 1 Heartbeat, 2 Camera, 3 Pose, 4 Collision, 5 Frame |
| 7 | 1 | u8 | `flags` | `0`; ignored by v1 receivers |
| 8 | 4 | u32 | `session` | from the host's `hello` |
| 12 | 4 | u32 | `seq` | sender's datagram counter: +1 per datagram, any kind, wraps |
| 16 | 8 | u64 | `time_us` | send time, section 5 |

A datagram of an unknown `kind` is ignored (it is not an error that ends the session), but it still
counts as proof of life for the watchdog when magic, version and session are right.

### 9.2 Heartbeat (kind 1)

Empty payload; the datagram is exactly 24 bytes.

### 9.3 Camera (kind 2), 152-byte payload, 176-byte datagram

The view the host is about to present. Send it once per rendered frame, sampled as close to
present as possible, so the guest renders with the camera the host will actually show.

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 64 | f32[16] | `view` | world to view, column-major |
| 64 | 64 | f32[16] | `proj` | view to clip, column-major |
| 128 | 4 | f32 | `fov_y` | vertical field of view, radians |
| 132 | 4 | f32 | `near` | near plane distance, metres, > 0 |
| 136 | 4 | f32 | `far` | far plane distance, metres; `+inf` for an infinite projection |
| 140 | 4 | u32 | `viewport_width` | pixels |
| 144 | 4 | u32 | `viewport_height` | pixels |
| 148 | 4 | u32 | `depth_range` | clip depth convention of `proj`, see below |

`depth_range`: `0` = OpenGL, near maps to -1 and far to +1; `1` = near 0, far 1 (Direct3D, Vulkan,
Metal); `2` = reversed, near 1 and far 0 (Bevy's default and many D3D engines). Other values are
malformed.

Offsets in this table and the following ones are relative to the payload, which starts at byte
24 of the datagram.

### 9.4 Pose (kind 3), 8 + 80·n byte payload

One or more entity poses. `count` is at least 1 and at most 102 (`(8192 - 24 - 8) / 80`; a full datagram is exactly 8192 bytes).

| Offset | Size | Type | Field |
|---|---|---|---|
| 0 | 4 | u32 | `count` |
| 4 | 4 | u32 | `reserved` |
| 8 + 80·i | 80 | PoseRecord | record `i` |

PoseRecord, 80 bytes:

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 4 | u32 | `entity` | sender's entity id |
| 4 | 4 | u32 | `flags` | bit 0 `TELEPORT`: do not interpolate from the previous pose; others 0 |
| 8 | 8 | u64 | `time_us` | when the pose was sampled, section 5 |
| 16 | 12 | f32[3] | `translation` | world position, m |
| 28 | 16 | f32[4] | `rotation` | world rotation, quaternion x, y, z, w |
| 44 | 12 | f32[3] | `scale` | |
| 56 | 12 | f32[3] | `velocity` | linear, world space, m/s |
| 68 | 12 | f32[3] | `angular_velocity` | world space axis times rad/s |

The datagram length must be exactly `24 + 8 + 80 * count`.

### 9.5 Collision (kind 4), 32 + 36·n byte payload

Triangles of the sender's world within `radius` of `center`, usually the host's static geometry
around the player, so the guest can collide with it. One snapshot is a **query**, split into
`chunk_count` datagrams with at most 226 triangles each (`(8192 - 24 - 32) / 36`).

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 4 | u32 | `query` | snapshot number, +1 per snapshot (serial arithmetic) |
| 4 | 2 | u16 | `chunk_index` | `0 .. chunk_count-1` |
| 6 | 2 | u16 | `chunk_count` | at least 1 |
| 8 | 12 | f32[3] | `center` | world, m |
| 20 | 4 | f32 | `radius` | m |
| 24 | 4 | u32 | `triangle_count` | triangles in this chunk, `0 ..= 226` |
| 28 | 4 | u32 | `reserved` | |
| 32 + 36·i | 36 | f32[9] | triangle `i` | vertices `a`, `b`, `c` as x, y, z each; world, CCW front |

The datagram length must be exactly `24 + 32 + 36 * triangle_count`. All chunks of a query carry the
same `center`, `radius` and `chunk_count`. A query with no triangles is one chunk with
`triangle_count = 0`; it means "nothing solid nearby". The receiver replaces its previous
collision set only once every chunk of a newer query has arrived.

### 9.6 Frame (kind 5), 24-byte payload, 48-byte datagram, and the pixel file

The sender renders, reads the image back to the CPU, writes it into a slot of the pixel file
announced by `frame_channel`, then sends `Frame`:

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 8 | u64 | `frame` | frame number, +1 per frame, starts at 1 |
| 8 | 4 | u32 | `slot` | slot written, `frame % slots` |
| 12 | 4 | u32 | `camera_seq` | header `seq` of the `Camera` the frame was rendered with, 0 if none |
| 16 | 4 | u32 | `width` | pixels |
| 20 | 4 | u32 | `height` | pixels |

`camera_seq` lets the host pair the pixels with the camera (and the poses) they were rendered for.

**Pixel file.** A regular file in the sender's temp directory, created and sized by the sender,
mapped read-write by the sender and read-only by the receiver. Layout:

File header, 64 bytes:

| Offset | Size | Type | Field | Value |
|---|---|---|---|---|
| 0 | 8 | u8[8] | `magic` | `"RBFRAME"` followed by one zero byte |
| 8 | 4 | u32 | `version` | `1` |
| 12 | 4 | u32 | `slots` | number of slots, 1 to 16; 3 is recommended |
| 16 | 4 | u32 | `width` | pixels, maximum per slot |
| 20 | 4 | u32 | `height` | pixels, maximum per slot |
| 24 | 4 | u32 | `stride` | bytes per row, `width * 4` |
| 28 | 4 | u32 | `format` | `1` = RGBA8, sRGB, straight (not premultiplied) alpha, rows top to bottom |
| 32 | 8 | u64 | `slot_size` | bytes per slot including its header, a multiple of 64 |
| 40 | 24 | | `reserved` | |

Slot `i` starts at `64 + i * slot_size`. Slot header, 64 bytes, followed by `stride * height`
pixel bytes:

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 8 | u64 | `lock` | seqlock counter, odd while the slot is being written |
| 8 | 8 | u64 | `frame` | frame number held |
| 16 | 8 | u64 | `time_us` | when the frame was rendered, section 5 |
| 24 | 4 | u32 | `camera_seq` | as in the datagram |
| 28 | 4 | u32 | `width` | pixels used, `<=` file width |
| 32 | 4 | u32 | `height` | pixels used, `<=` file height |
| 36 | 28 | | `reserved` | |
| 64 | | u8[] | pixels | `stride * height` bytes |

`lock` is 8-byte aligned in the file and must be accessed with atomic 64-bit operations.

Writer:
1. Store `lock + 1` (now odd), then issue a release fence
   (`std::atomic_thread_fence(std::memory_order_release)`).
2. Write the slot header fields and the pixels.
3. Store `lock + 1` again (even), with release ordering.
4. Send the `Frame` datagram.

Reader, on a `Frame` datagram:
1. `l1 = lock` (acquire). If `l1` is odd, skip this frame.
2. Copy the header fields and pixels out.
3. Acquire fence, then `l2 = lock`. If `l2 != l1` or the copied `frame` is not the datagram's
   `frame`, the slot was overwritten while copying: discard the copy and wait for the next frame.

With 3 or more slots the writer rarely reaches the slot being read.

The sender deletes the file at the end of the session; a receiver never deletes it, and unmaps and
closes it when the session ends or a new `frame_channel` replaces it. On Windows:

- both sides open the file with `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`, so the
  receiver can open it while the sender has it, and the sender can delete it while the receiver
  still has it mapped (Windows removes the file once the last handle and view close);
- the sender unmaps its own view (`UnmapViewOfFile`, then closes the mapping and file handles)
  before deleting, since Windows refuses to delete or resize a file the calling process maps;
- a file cannot be resized while any process maps it, so a sender never reuses a path a receiver
  may still map: a new size or a new session gets a new file and a new `frame_channel`.

A sender that crashes leaves its file behind. Names should contain the sender's process id, so a
later run can tell its own files from a dead process's.

## 10. C layouts

All fields little-endian. These structs have no padding on any common ABI; the `static_assert`s
check that.

```c
#include <stdint.h>
#include <assert.h>

#define RBL_MAGIC          0x4B4C4252u   /* "RBLK" read as little-endian u32 */
#define RBL_VERSION        1
#define RBL_MAX_DATAGRAM   8192
#define RBL_MAX_POSES      102
#define RBL_MAX_TRIANGLES  226

enum rbl_kind { RBL_HEARTBEAT = 1, RBL_CAMERA = 2, RBL_POSE = 3, RBL_COLLISION = 4, RBL_FRAME = 5 };
enum rbl_depth_range { RBL_DEPTH_NEG_ONE_TO_ONE = 0, RBL_DEPTH_ZERO_TO_ONE = 1, RBL_DEPTH_REVERSED = 2 };
#define RBL_POSE_TELEPORT  1u

typedef struct rbl_header {      /* 24 bytes, offset 0 of every datagram */
    uint32_t magic;              /*  0 */
    uint16_t version;            /*  4 */
    uint8_t  kind;               /*  6 */
    uint8_t  flags;              /*  7 */
    uint32_t session;            /*  8 */
    uint32_t seq;                /* 12 */
    uint64_t time_us;            /* 16 */
} rbl_header;

typedef struct rbl_camera {      /* 152 bytes, after the header */
    float    view[16];           /*   0 */
    float    proj[16];           /*  64 */
    float    fov_y;              /* 128 */
    float    near_m;             /* 132 */
    float    far_m;              /* 136 */
    uint32_t viewport_width;     /* 140 */
    uint32_t viewport_height;    /* 144 */
    uint32_t depth_range;        /* 148 */
} rbl_camera;

typedef struct rbl_pose_header { /* 8 bytes, after the header, then `count` rbl_pose */
    uint32_t count;              /* 0 */
    uint32_t reserved;           /* 4 */
} rbl_pose_header;

typedef struct rbl_pose {        /* 80 bytes */
    uint32_t entity;             /*  0 */
    uint32_t flags;              /*  4 */
    uint64_t time_us;            /*  8 */
    float    translation[3];     /* 16 */
    float    rotation[4];        /* 28, x y z w */
    float    scale[3];           /* 44 */
    float    velocity[3];        /* 56 */
    float    angular_velocity[3];/* 68 */
} rbl_pose;

typedef struct rbl_collision {   /* 32 bytes, after the header, then `triangle_count` rbl_triangle */
    uint32_t query;              /*  0 */
    uint16_t chunk_index;        /*  4 */
    uint16_t chunk_count;        /*  6 */
    float    center[3];          /*  8 */
    float    radius;             /* 20 */
    uint32_t triangle_count;     /* 24 */
    uint32_t reserved;           /* 28 */
} rbl_collision;

typedef struct rbl_triangle {    /* 36 bytes */
    float    v[3][3];            /* a, b, c; each x y z */
} rbl_triangle;

typedef struct rbl_frame {       /* 24 bytes, after the header */
    uint64_t frame;              /*  0 */
    uint32_t slot;               /*  8 */
    uint32_t camera_seq;         /* 12 */
    uint32_t width;              /* 16 */
    uint32_t height;             /* 20 */
} rbl_frame;

typedef struct rbl_frame_file {  /* 64 bytes at offset 0 of the pixel file */
    char     magic[8];           /*  0, "RBFRAME\0" */
    uint32_t version;            /*  8 */
    uint32_t slots;              /* 12 */
    uint32_t width;              /* 16 */
    uint32_t height;             /* 20 */
    uint32_t stride;             /* 24 */
    uint32_t format;             /* 28 */
    uint64_t slot_size;          /* 32 */
    uint8_t  reserved[24];       /* 40 */
} rbl_frame_file;

typedef struct rbl_frame_slot {  /* 64 bytes at 64 + i * slot_size, pixels follow */
    uint64_t lock;               /*  0, atomic */
    uint64_t frame;              /*  8 */
    uint64_t time_us;            /* 16 */
    uint32_t camera_seq;         /* 24 */
    uint32_t width;              /* 28 */
    uint32_t height;             /* 32 */
    uint8_t  reserved[28];       /* 36 */
} rbl_frame_slot;

static_assert(sizeof(rbl_header) == 24, "header");
static_assert(sizeof(rbl_camera) == 152, "camera");
static_assert(sizeof(rbl_pose_header) == 8, "pose header");
static_assert(sizeof(rbl_pose) == 80, "pose");
static_assert(sizeof(rbl_collision) == 32, "collision");
static_assert(sizeof(rbl_triangle) == 36, "triangle");
static_assert(sizeof(rbl_frame) == 24, "frame");
static_assert(sizeof(rbl_frame_file) == 64, "frame file");
static_assert(sizeof(rbl_frame_slot) == 64, "frame slot");
```

On a big-endian machine every multi-byte field must be byte-swapped; the structs above then only
describe offsets.

## 11. Worked example

A `Heartbeat` from the guest in session `0xABCD0001`, its 5th datagram (`seq = 4`), sent at
`time_us = 1_700_000_000_000_000`:

```
52 42 4C 4B  01 00  01  00  01 00 CD AB  04 00 00 00  00 40 1E 18 24 0A 06 00
magic "RBLK" ver=1 kind flg session      seq           time_us
```

A minimal host loop, in order: accept TCP, read the guest's `hello`, reply with yours, then every
frame send `Camera` (and `Collision` when the player has moved), read all pending datagrams and
control lines, send `Heartbeat` when `heartbeat_ms` has passed, and check the watchdog.

## 12. Versioning of this document

- v1: first release.
- v1, revision 2: section 6.1 (half-close and drain after `bye`; a received `bye` wins over a
  following reset) and the Windows file-sharing rules for the pixel file. No layout or message
  changed, so the protocol version stays 1.

A change that alters any layout or meaning above needs a new protocol version. Adding a control
message type or field, or a new datagram `kind`, does not, because receivers ignore what they do
not know.
