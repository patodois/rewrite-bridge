# Passthrough link contract, protocol v1

This is revision 6 of the contract for protocol v1 (section 12 lists what each revision changed). This
document is the whole protocol. A host plugin written in C, C++, C# or anything else should be
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
| pixels | memory-mapped file | binary, little-endian | RGBA frames, optional linear depth, announced by `Frame` |

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
- **Authentication (revision 6, optional).** Any local process can connect to a port on
  `127.0.0.1`. A host that wants only the guest of its own user's choosing configures a token,
  hands it to the guest through an owner-only endpoint file, and refuses a `hello` without it
  (section 6.2). Without a token the link works as in revision 5.
- **State, UDP.** Each side binds its own UDP socket on `127.0.0.1` (port configurable; `0` means
  any free port) and announces the port in its `hello`. A side sends datagrams to
  `127.0.0.1:<peer udp_port>` and accepts datagrams only from that exact address. Anything else is
  dropped.
- **Datagram size.** A datagram is at most **8192 bytes** (`MAX_DATAGRAM`). macOS refuses UDP
  datagrams above 9216 bytes by default, and 8192 leaves room. Larger data is chunked (see
  `Collision`) or goes through the pixel file (see `Frame`).
  A receiver reads into a buffer of at least **8193 bytes**: the OS cuts a datagram that does not fit
  and says nothing, so with a buffer of exactly 8192 a 9000 byte `Collision` would arrive as a valid
  8192 byte one. A datagram of more than 8192 bytes whose kind is known is malformed (no kind's
  length rule allows it) and dropped; one of an unknown kind is ignored like any other unknown kind
  (section 9.1), at whatever length the receiver's buffer holds.
- UDP may drop or reorder datagrams even on loopback when a receive buffer fills. State is
  therefore "latest wins": every datagram is self-contained, and stale ones are discarded (section 7).
- **Receive buffer.** A receiver SHOULD ask the OS for a UDP receive buffer of at least **4 MiB**
  (`setsockopt(SO_RCVBUF)`; Java: `StandardSocketOptions.SO_RCVBUF`) when it binds its socket, and
  ignore a failure or a smaller grant: the OS default is often under 1 MiB, which a snapshot of
  `Collision` chunks (section 9.5) can exceed. Pacing on the sender's side is the other half.

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
- Quaternions are `x, y, z, w`, unit length when a sender writes them. A receiver that needs a unit
  quaternion normalises it (section 9.4 says what a decoder does with the others).

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
   A guest that has a token for this host adds it as `"token":"..."` (section 6.2).
3. The host checks it:
   - if the host has a token (section 6.2), the `token` must match it; this check comes first, and a
     `hello` that fails it gets `{"type":"bye","reason":"unauthorized"}`;
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

**What counts before and after the handshake (revision 5).** Nothing is ignored for being early or
late:

- The first line a side receives must be a `hello`, or a `bye` (a host answers a connection it cannot
  take with `bye` before reading anything, so a guest can get a `bye` first; the receiver closes as
  in section 6.1 without a `bye` of its own, and the handshake has failed). Any other first line, a
  known type such as `spawn` or `input` as much as an unknown type, is a protocol error: the receiver
  sends `bye` with a reason and closes (section 6.1). The sender's `hello` has not been read, so
  nothing it sent after that line counts.
- A second `hello` on a connection whose handshake has completed is a protocol error: the receiver
  sends `bye` and the session is over. (A `hello` cannot renegotiate anything: the version and the
  session are fixed for the life of the connection.)
- A `hello` that fails its checks (section 8, `hello`) is a handshake error: `bye` and close.
- A guest may reconnect after a session ends and the reference implementations do, after a delay
  they let the application set; a host accepts a new guest as soon as the old session is over.
  Whether a guest reconnects is its own choice, so no vector requires it.

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

*Implementation note, no rule changes.* Under wasmtime closing a socket waits until the runtime has delivered the
bytes it still holds for it to the kernel, which does not happen while the peer neither reads nor closes (5 s on
macOS, for ever on Linux). An implementation there cannot meet the 200 ms of step 3 for such a peer without
blocking its loop. The one in `rewrite-bridge` sets that socket aside and closes it when the runtime lets go, so
for a peer that holds its socket open and stops reading, step 4 comes late. It also takes no new connection while
four such sockets are set aside: a host then does not answer a second connection with `busy` (section 2), the
connection waits unanswered in the system's backlog.

Future versions keep the datagram header's first 8 bytes (magic, version, kind, flags) and the
`hello` fields above, so two implementations can always negotiate.

### 6.2 Authentication: the token and the endpoint file (revision 6)

Authentication is optional and decided by the host alone. A host that has no token accepts any guest,
exactly as before revision 6; a guest that has no token sends none.

**The token.** A token is a string of **1 to 256 bytes** in UTF-8.

1. A host may be configured with a token. A guest that has been given the host's token sends it in its
   `hello` as the string field `token` (section 8). Only a guest's `hello` carries one: a host never
   puts `token` in its own `hello`.
2. A host with a token checks it as soon as the guest's `hello` has been parsed, **before any other
   check of section 6**: the `hello` must carry a `token` of 1 to 256 bytes that is equal, byte for
   byte in UTF-8, to the host's. If the field is absent (or `null`), outside that length, or not equal,
   the host sends `{"type":"bye","reason":"unauthorized"}` and closes as in section 6.1. This is a
   handshake error. A peer that did not show the token learns only that word, not which check failed
   nor anything else about the host (its version range, for instance, which a later check would name).
3. A host without a token **ignores** `token`: it parses the field like any other (a `token` that is not
   a string is a protocol error, section 8) and does not look at its value or its length.
4. A guest parses a `token` in a host's `hello` the same way and ignores it.
5. The token is a secret. A host never echoes it, logs it, or puts it, or any part of the token a guest
   sent, into a `bye` reason, an error, an event or a log line; neither does a guest. The fixed reason
   `unauthorized` is the whole answer.
6. **Comparison in constant time.** The time the comparison takes must not depend on the contents of
   either token, nor on where they first differ. The reference method: copy each token into a buffer of
   256 zero bytes, OR together the XOR of all 256 byte pairs, OR in the XOR of the two lengths, and call
   the tokens equal only if the result is 0. There is no early exit, and a length mismatch does the same
   work as a match. (The length itself is not secret: a generated token always has 64 bytes.)
7. **Generating.** A host that makes its own token takes **32 bytes from the operating system's
   cryptographic random number generator** (`getrandom`, `getentropy` or `arc4random_buf`, or
   `/dev/urandom`, on Unix; `BCryptGenRandom` on Windows; `SecureRandom` in Java;
   `RandomNumberGenerator` in .NET) and writes them as **64 lowercase hex digits**. It makes a new one
   each time it starts. Receivers do not check the format: any string of 1 to 256 bytes is a token, so a
   host may also be given one by a launcher.

   *Implementation note, no rule changes.* A WebAssembly component has no operating system to ask: on WASI the
   generator is the runtime's `wasi:random/random`. In wasmtime 50 that is a thread generator seeded from the
   operating system, and an embedder can replace it (even with a fixed sequence), so the rule holds on WASI as
   far as the runtime keeps it. The other WASI source, `insecure-seed`, may be deterministic and is never used for
   a token or a session number.

**The endpoint file.** A host hands the port and the token to the guest through a file that only the
user who runs both can read. (The idea follows the endpoint file that Horizon writes for its control
socket; the format here is this contract's own.) The file holds one JSON object:

```json
{"control_port":47800,"token":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"}
```

| Field | Type | Meaning |
|---|---|---|
| `control_port` | integer, valid only **1 ..= 65535** | the port the host is listening on, the one it actually bound (with port 0 configured, the port the OS gave it) |
| `token` | string, **1 ..= 256 bytes** | the host's token |

The text follows the rules of a control line (section 8, "Syntax of a line"), is at most 4096 bytes
and may end in a newline. Unknown fields are ignored, so a later revision can add some. A reader
refuses a file that breaks any of this.

- **Writing (host).** The host writes the file **only after it has bound its control port**, and with
  the port it bound. It writes atomically: it creates a temporary file in the same directory,
  exclusively and with owner-only permissions from the moment it exists (Unix:
  `open(O_CREAT | O_EXCL | O_WRONLY, 0600)`, never a `chmod` after the data is in), writes and closes
  it, then renames it over the path (Windows: `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`). A reader
  therefore finds no file, the previous one, or the complete new one, never a partial one. On Windows
  there are no mode bits to set: the file goes under the user's local application data (below), and the
  per-user ACL of that directory is what keeps other users out. The host removes the file when it shuts
  down cleanly. A host that crashes leaves it behind; its token dies with it, since the next run makes a
  new one.

  *Implementation note, no rule changes.* A WebAssembly component on WASI cannot ask for a mode when it
  creates a file: the file gets what the host process's umask leaves (on a Unix host, 0666 minus the umask).
  Measured from such a writer: umask 022 gives 0644, 077 gives 0600, 000 gives 0666. The rule above stands;
  such an implementation can only create the file exclusively, and it should tell its operator the
  consequences. Under the default umask 022 the token is readable by other users, and a guest that checks
  the mode (the rule below) refuses the file, so that host cannot serve it. A component that reads the file
  cannot check the mode either, so for it the check below is lost. `umask 077` for the process makes the file
  0600, and then both a native guest and a component guest work. A folder that only the user can enter (0700)
  keeps other users away from the token, but the file in it is still 0644 under umask 022, so a native guest
  still refuses it with `PermissionDenied` (measured). On a Windows host no umask applies: the folder's
  per-user ACL is what keeps other users out, as above. This is a deviation the platform forces, not a
  relaxation of the rule for a platform that can set the mode.
- **Reading (guest).** The guest reads the file and takes `control_port` and `token` from it. It refuses
  a path that does not name a regular file (symbolic links followed), and checks that before it opens the
  path, since opening a FIFO for reading would block. On Unix it also refuses a file whose permissions
  grant anything to the group or to others (`mode & 077` not 0): such a file can have been written by
  someone else. Its errors do not quote the file's contents. A file that does not
  exist means that the host is not running (yet): the guest tries again later. A guest that is answered
  `unauthorized`, or cannot connect, reads the file again before it retries, since the host may have
  restarted with a new port and token.
- **Where.** The path is the application's choice; host and guest only have to agree on it. The
  recommended places, with `<name>` a name both sides know (the host's game, for instance):
  - Linux and other Unix: `$XDG_RUNTIME_DIR/rewrite-link/<name>.json` (that directory belongs to the
    user, has mode 0700 and is emptied at logout); without `XDG_RUNTIME_DIR`,
    `$HOME/.cache/rewrite-link/<name>.json`;
  - macOS: `$HOME/Library/Application Support/rewrite-link/<name>.json`;
  - Windows: `%LOCALAPPDATA%\rewrite-link\<name>.json`.

  An application that creates the `rewrite-link` directory creates it with mode 0700 on Unix.

**What the token protects, and what it does not.**

- It keeps out **other users** of the machine, and **any process that cannot read the endpoint file**: a
  sandboxed program, a service running under another account, anything that can open a TCP connection
  to `127.0.0.1` but cannot read the user's files. That holds when the file is owner-only; a host that
  cannot create it with mode 0600 (the implementation note under "Writing") keeps others out only as far as
  its umask or its folder does.
- It defeats a process that **squats the control port** before the host starts. The host's bind then
  fails, so the host writes no file, and a guest that reads the file never hands the token to the
  squatter. (A host that wrote the file before binding would send the guest to whoever holds the port,
  token included.) A file left by a crashed host can still lead a guest to a process that took the port
  since; what that process receives is the dead host's token, which opens nothing.
- It does **not** protect against processes running **as the same user**: they can read the file, and
  they can read the memory of the host and the guest anyway. Nor against an administrator or root.
- It authenticates the guest to the host, not the host to the guest: a guest that reads the file trusts
  that the process listening on the port named there is the host, which holds for a file the host wrote
  after binding and that only the user can write.
- It does not hide that a host is there. Anyone can still connect and get `busy` while a session is on,
  or `unauthorized`.
- It does not encrypt or sign anything. The control connection and the datagrams travel in clear on
  loopback, as before; what the token decides is who may start a session.

**UDP is unchanged.** Each side accepts datagrams only from the address the peer announced in its
`hello` and only with the session from the host's `hello` (section 2, section 6). With a token those two
announcements arrive over a control connection that has been authenticated, and that is what binds the
UDP traffic to the authenticated peer: another process would have to send from the guest's own UDP port,
which the guest holds, and know the session number.

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
  - `Frame`: drop it unless its `frame` number is newer than the newest one shown *from the
    current pixel file*; a new `frame_channel` resets that number (section 8).
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

### Syntax of a line (revision 5)

A line is JSON as RFC 8259 defines it, with these points settled. They apply to the whole line,
including the parts of unknown fields that nobody reads: a line that breaks one of them is a protocol
error wherever in the line it does.

1. **Framing.** A line ends at `\n`. One `\r` right before it is removed. Any other `\r` is JSON
   whitespace like space, tab and `\n`; no other character (form feed, vertical tab, U+00A0, a NUL
   byte) is whitespace.
2. **Encoding.** The line is valid UTF-8 (no overlong forms, no surrogates, nothing above U+10FFFF) and
   does not start with a byte order mark: `EF BB BF` is not whitespace, so a line that starts with it
   is not JSON.
3. **Nesting.** Arrays and objects nest at most **127** deep, the line's own object counting as the
   first. So `{"x":[[...]]}` with 126 arrays is accepted and with 127 is not. Without a limit a line of
   65536 bytes could nest 32768 deep and overflow the stack of a recursive parser.
4. **Strings.** Every `\u` escape names a Unicode scalar value. A surrogate must come as a high
   surrogate (`D800`..`DBFF`) followed at once by a low one (`DC00`..`DFFF`); a lone surrogate, a pair
   the wrong way round and a high surrogate followed by anything but a low one are errors. **`\u0000`
   is an error** in any string, key or value: it cannot be held by every implementation (the C one
   works on NUL-terminated strings, where `"a\u0000b"` would silently become `"a"`, and a `path` could
   be cut short), so none accepts it. (A raw control character in a string is an error in JSON
   already.) A sender never writes one: a library that is handed a string with U+0000 in it writes
   U+FFFD in its place (a C caller cannot hand one over, its strings end at the first NUL).
5. **Duplicate keys.** When a key appears more than once in an object, **the last one wins**, in every
   object. Keys are compared after their escapes are decoded (`"\u0065ntity"` is `"entity"`). A line
   with `{"type":"despawn","type":"bye"}` is a `bye`. The earlier values still have to be valid:
   `{"x":"\u0000","x":1}` is an error although `x` ends up as `1`.
6. **Integers.** A field the tables below call an integer takes an integer literal: digits only, with no
   sign (not even `-0`), no fraction and no exponent. `7.0`, `7e0` and `-0` are not integers here,
   whatever their value, and neither is a string, `true` or `null`. A value outside the field's parseable
   range (tables below) is an error; so is one too large for any machine integer (`1e400`, 30 digits).
7. **`null`.** For a field marked "optional", `null` means the field is absent. For any other field it
   is the wrong type and an error. An encoder omits an absent optional field rather than writing `null`.
8. **Unknown** types and fields are ignored after all of the above has been checked: ignoring a field
   means not using its value, not skipping its syntax.

### `hello`

| Field | Type | Meaning |
|---|---|---|
| `role` | `"host"` / `"guest"` | sender's role |
| `game` | string | sender's game, e.g. a `Provider::game()` name |
| `protocol` | integer, `0 ..= 65535` | highest supported version (in the host reply: the chosen version) |
| `min_protocol` | integer, `0 ..= 65535` | lowest supported version |
| `udp_port` | integer, parses as `0 ..= 65535`, valid only **1 ..= 65535** | the sender's UDP port on 127.0.0.1 |
| `heartbeat_ms` | integer, parses as `0 ..= 4294967295`, valid only **10 ..= 10000** | how often the sender sends `Heartbeat` |
| `timeout_ms` | integer, `0 ..= 4294967295` (parses and valid) | the sender's own watchdog timeout |
| `session` | integer, parses as `0 ..= 4294967295`; valid only **1 ..= 4294967295** from a host | session number; a guest's `hello` does not need one (see below) |
| `token` | string, optional; parses as any string, valid only **1 ..= 256 bytes** (UTF-8) | revision 6: the host's token, sent by a guest (section 6.2); a host's `hello` never has one |

Each integer has two ranges. The **parseable** range is the type (`u16` or `u32`): a value outside it
is a protocol error found when the line is read. The **valid** range is what the handshake accepts: a
value inside the type but outside it is a handshake error. Both end in a `bye`; the vectors tell them
apart (`ctl.*` shows what parses, `sess.*` what the handshake accepts).

A guest's `hello` may carry a `session`. A receiver parses it like the host's, as an optional
`u32` (`null` is absent, `-1` or a string is a protocol error), and ignores it: the session number is
the host's to choose.

`token` follows the same two-range rule, with lengths in bytes instead of values. Any string parses
(`null` is absent; a number, `true`, an array or an object is a protocol error). The valid range,
1 to 256 bytes of UTF-8, is checked by a host that has a token, and only there: a `token` outside it,
like a missing or unequal one, is a handshake error answered with `unauthorized` (section 6.2). A host
without a token, and a guest, ignore the value whatever its length.

The two version fields are `u16`, like the `version` of the datagram header they end up in: `65536`
is out of range, not a version nobody supports (as a `u32` it would make a `hello` with `protocol`
65536 and `min_protocol` 1 negotiate version 1).

A `hello` that breaks a range is a handshake error: the receiver sends `bye` with the reason and
closes (section 6). Two ranges carry more than the type does:

- `heartbeat_ms` outside `10 ..= 10000`. Without a floor a peer could ask for a heartbeat every
  millisecond or, with a huge value, stretch the watchdog (`4 * heartbeat_ms`) for ever. A side whose
  own configuration is outside the range sends the nearest value inside it.
- `udp_port` 0. A socket bound to "any free port" announces the port it got, never 0, and nothing
  can be sent to port 0.

`timeout_ms` has no range beyond its type, 0 included, because a receiver never uses it: the
watchdog that applies to a peer is `max(own timeout_ms, 4 * peer heartbeat_ms)` (section 7) and
the peer's `timeout_ms` is informative (logs, diagnostics). A receiver must not divide by it, wait
for it or trust it for anything.

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

A receiver keeps pose state (section 9.4) **only for entities the peer has spawned and not
despawned**:

- a `Pose` record for any other id is ignored and counted (the Rust `Stats::unknown_entity`); a
  peer cannot make the receiver allocate by inventing ids;
- a peer has at most **1024** live spawned entities. A `spawn` of a new id beyond that is ignored
  and logged (once per session is enough) and its poses are ignored too; a `spawn` of an id that is
  already live always replaces it, and a `despawn` frees the place. Nothing is sent to the peer about
  an ignored `spawn`: there is no message for it, and the peer finds out from the missing poses.

### `despawn`

`{"type":"despawn","entity":7}` removes one of the sender's entities.

A `despawn` of an id that is not live (never spawned, already despawned, or a `spawn` that was ignored
because of the limit above) is ignored: no event, no error, nothing sent. Treating it as an error
would make a peer whose `spawn` was dropped end the session by despawning that entity.

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

| Field | Type | Meaning |
|---|---|---|
| `path` | string | absolute path of the pixel file |
| `width`, `height` | integer, `0 ..= 4294967295` | the file's maximum frame size; the limits are checked when the file is opened (section 9.6) |
| `slots` | integer, `0 ..= 4294967295` | the file's number of slots; likewise |

`path` is an absolute path to a file the receiver may map read-only (it is opened as section 9.6
requires). A later `frame_channel` replaces the previous one (for example after a resize).

Each pixel file numbers its frames from 1, so a writer starts again at 1 for a new file. A receiver
therefore **resets its frame staleness (the newest frame number it has shown) for the channel when
a `frame_channel` replaces the previous one**; without that it would drop the new file's frames
until their numbers passed the old file's. A `Frame` datagram of the old file that was still in
flight when the line arrived can be delivered after the reset; the receiver's read then finds that
slot holding another frame (or the old file gone) and skips it.

### `bye`

`{"type":"bye","reason":"shutting down"}`. The sender then half-closes, drains and closes the
connection (section 6.1). The host also answers a connection it cannot take with
`{"type":"bye","reason":"busy"}`, before or without reading that client's `hello`.

| Field | Type | Meaning |
|---|---|---|
| `reason` | string, optional | why the session ends, for people and logs |

A `bye` without `reason`, or with `"reason": null`, is valid and means the empty reason. A `reason` of
another type (a number, `true`, an array, an object) is a protocol error like any wrong-typed optional
field, and gets a `bye` in answer. A `bye` ends the session; extra fields do not matter.

**The text of a reason is not part of the protocol.** Two reasons are fixed: `"busy"`, the host's refusal
of a second guest (section 2), and, from revision 6, `"unauthorized"`, the host's refusal of a `hello`
without the right token (section 6.2). A guest may act on these two (after `unauthorized`, read the
endpoint file again). A receiver must not branch on any other text (it may show it); a sender that
refuses a handshake or ends a session on a protocol error says why in a sentence of its own, and
keeps it short (it must fit the 65536 byte line).

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
counts as proof of life for the watchdog when magic, version and session are right and it is at least
24 bytes long (the header). The values of the fields of a known
kind are not checked beyond what each section below names: a decoder tells a datagram that is
malformed in its layout (length, counts, `depth_range`, indices) from one that is merely strange,
and delivers the strange one as sent.

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

**Values.** A sender writes finite numbers (only `far` may be `+inf`), `near > 0`, `far > near`, nonzero
viewport sizes and the matrices it really renders with. A decoder checks none of it; a `Camera` with
`near` 0, a NaN, a `far` below `near` or a viewport of 0 is a valid datagram and is delivered as sent.
A receiver that builds a projection or a render target from it checks first, and ignores a camera
that fails the way it ignores a stale one. The sizes are `u32` and can be 4294967295: never allocate
from them unchecked.

`proj` is the projection with +y up in normalized device coordinates: pixel row 0 of an image
rendered with it is NDC y = +1, whatever the host's graphics API does to its own framebuffer (a host
whose API flips Y flips it back before sending). Section 9.7 relies on this to reproject frames.

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
| 4 | 4 | u32 | `flags` | bit 0 `TELEPORT`: do not interpolate from the previous pose; others 0, ignored on read |
| 8 | 8 | u64 | `time_us` | when the pose was sampled, section 5 |
| 16 | 12 | f32[3] | `translation` | world position, m |
| 28 | 16 | f32[4] | `rotation` | world rotation, quaternion x, y, z, w |
| 44 | 12 | f32[3] | `scale` | |
| 56 | 12 | f32[3] | `velocity` | linear, world space, m/s |
| 68 | 12 | f32[3] | `angular_velocity` | world space axis times rad/s |

The datagram length must be exactly `24 + 8 + 80 * count`. A record whose `entity` the peer has not
spawned (section 8, `spawn`) is ignored.

**Values.** A receiver ignores the `flags` bits it does not know, whatever they are, and the decoder
delivers the field as sent (a later revision may give a bit a meaning, with 0 keeping the old one). A
sender writes finite numbers and a `rotation` of unit length; a `scale` of 0 or a negative one is
valid (a hidden or mirrored entity). A decoder checks none of it, NaN and an all-zero `rotation`
included. A receiver that applies a pose should normalise the rotation first and skip a record whose
numbers are not finite or whose rotation has length 0.

### 9.5 Collision (kind 4), 32 + 36·n byte payload

Triangles of the sender's world within `radius` of `center`, usually the host's static geometry
around the player, so the guest can collide with it. One snapshot is a **query**, split into
`chunk_count` datagrams with at most 226 triangles each (`(8192 - 24 - 32) / 36`).

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 4 | u32 | `query` | snapshot number, +1 per snapshot (serial arithmetic) |
| 4 | 2 | u16 | `chunk_index` | `0 .. chunk_count-1` |
| 6 | 2 | u16 | `chunk_count` | `1 ..= 4096` (`MAX_COLLISION_CHUNKS`) |
| 8 | 12 | f32[3] | `center` | world, m |
| 20 | 4 | f32 | `radius` | m |
| 24 | 4 | u32 | `triangle_count` | triangles in this chunk, `0 ..= 226` |
| 28 | 4 | u32 | `reserved` | |
| 32 + 36·i | 36 | f32[9] | triangle `i` | vertices `a`, `b`, `c` as x, y, z each; world, CCW front |

The datagram length must be exactly `24 + 32 + 36 * triangle_count`. All chunks of a query carry the
same `center`, `radius` and `chunk_count`. A query with no triangles is one chunk with
`triangle_count = 0`; it means "nothing solid nearby". The receiver replaces its previous
collision set only once every chunk of a newer query has arrived.

**Size limit (revision 5).** A query has at most **4096 chunks** (`MAX_COLLISION_CHUNKS`), that is at
most `4096 * 226 = 925696` triangles, 32 MiB in the 36 byte form of the wire. A `chunk_count` above 4096
makes the datagram malformed, and a decoder drops it. Before, `chunk_count` could be 65535, and a
sender or a hostile peer could make a receiver hold 533 MB of triangles for one snapshot (the Rust and
Java implementations did; the C and C# ones capped the sum at 2097152 triangles on their own). The limit is
generous for what the link is for: a snapshot of the geometry around a player is some thousands to
some hundred thousand triangles, and it takes 128 ticks of pacing (below) to send 4096 chunks. A sender
whose snapshot needs more chunks does not send it; it keeps the triangles nearest to `center` and
sends those. A receiver may impose a smaller limit of its own, never a larger one.

**Assembly.** A receiver keeps the number of the newest query it has completed and at most one query
being assembled. For a chunk (that passed the checks above and in section 9):

1. If it has completed a query and the chunk's `query` is not newer (serial arithmetic), the chunk is
   stale and dropped. A chunk of a query that was just completed is stale for the same reason.
2. If the query being assembled is the chunk's and the chunk's `chunk_count` differs from the one the
   assembly was started with, the chunk is malformed and dropped, and the assembly goes on (it is not
   discarded, and the chunk does not start a new one).
   If the query being assembled is newer than the chunk's, the chunk is stale and dropped.
3. Otherwise the chunk starts a new assembly, which discards the incomplete older one. The assembly
   takes `query`, `chunk_count`, `center` and `radius` **from this chunk, the first one that arrived**
   (not necessarily index 0). Later chunks of the query that differ in `center` or `radius` are not
   compared and their values are not used.
4. A chunk whose `chunk_index` is already filled is dropped: **the first copy of a chunk wins** and
   a repeat counts for nothing, so a datagram delivered twice cannot complete a query early. A new
   index is stored.
5. When every index `0 .. chunk_count-1` is filled, the receiver emits the snapshot, with the
   triangles in chunk index order, and the query becomes the newest completed.

The rules are the same for a host and a guest. They make the outcome of a snapshot whose chunks
disagree deterministic: the first chunk to arrive decides `center`, `radius` and `chunk_count`; a
chunk with another `chunk_count` is lost; the snapshot completes when the right number of distinct
chunks has arrived.

**Pacing.** A sender SHOULD NOT put a whole snapshot on the wire at once: it queues the chunks and
sends at most **32** collision datagrams per tick (per `poll()` in the reference implementations),
so the burst never exceeds what a receive buffer holds (32 full chunks are 256 KiB; a 400-chunk
snapshot of 3.2 MB sent in one call is mostly lost on an OS default buffer). A snapshot queued
while an older one still has chunks waiting **replaces** them, since the receiver discards an
incomplete older query when a newer one starts anyway. Heartbeats, cameras and poses are not held
back by the queue.

### 9.6 Frame (kind 5), 24-byte payload, 48-byte datagram, and the pixel file

The sender renders, reads the image back to the CPU (and, if the file has a depth plane, the depth
too), writes it into a slot of the pixel file announced by `frame_channel`, then sends `Frame`:

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 8 | u64 | `frame` | frame number, +1 per frame, starts at 1 in each pixel file; **0 is malformed** |
| 8 | 4 | u32 | `slot` | slot written, `frame % slots` |
| 12 | 4 | u32 | `camera_seq` | header `seq` of the `Camera` the frame was rendered with, 0 if none |
| 16 | 4 | u32 | `width` | pixels |
| 20 | 4 | u32 | `height` | pixels |

A `Frame` with `frame` 0 is malformed and dropped by the decoder: a slot nobody has written yet holds
frame 0, so a datagram that named frame 0 would match it and deliver an empty image. The other
fields are not checked by the decoder; the reader below checks them against the file.

`camera_seq` lets the host pair the pixels with the camera (and the poses) they were rendered for;
section 9.7 says how it uses them to hide latency.

**Pixel file.** A regular file in the sender's temp directory, created exclusively and sized by the
sender, mapped read-write by the sender and read-only by the receiver. The receiver treats the
file as untrusted input (see "Opening and creating" below). Layout:

File header, 64 bytes:

| Offset | Size | Type | Field | Value |
|---|---|---|---|---|
| 0 | 8 | u8[8] | `magic` | `"RBFRAME"` followed by one zero byte |
| 8 | 4 | u32 | `version` | `1` |
| 12 | 4 | u32 | `slots` | number of slots, 1 to 16; 3 is recommended |
| 16 | 4 | u32 | `width` | pixels, maximum per slot, 1 to 16384 (`width * height` at most 67108864) |
| 20 | 4 | u32 | `height` | pixels, maximum per slot, 1 to 16384 (`width * height` at most 67108864) |
| 24 | 4 | u32 | `stride` | bytes per row, exactly `width * 4` |
| 28 | 4 | u32 | `format` | `1` = RGBA8, sRGB, straight (not premultiplied) alpha, rows top to bottom |
| 32 | 8 | u64 | `slot_size` | bytes per slot including its header and depth plane, a multiple of 64 |
| 40 | 4 | u32 | `depth_format` | `0` = no depth plane; `1` = `f32` linear view depth in metres (below); readers treat other values as `0` |
| 44 | 4 | u32 | `depth_stride` | bytes per depth row, `width * 4`; `0` when there is no depth plane |
| 48 | 8 | u64 | `depth_offset` | byte offset of the depth plane from the start of the slot: a multiple of 64 and at least `64 + stride * height`; `0` when there is no depth plane |
| 56 | 8 | | `reserved` | |

Slot `i` starts at `64 + i * slot_size`. Slot header, 64 bytes, followed by `stride * height`
pixel bytes and, when `depth_format` is `1`, the depth plane at `depth_offset`:

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
| `depth_offset` | | f32[] | depth | `depth_stride * height` bytes, only if `depth_format` is `1` |

```
slot start  +0              slot header           64 bytes
            +64             colour rows           stride * height
            +depth_offset   depth rows            depth_stride * height   (depth_format 1 only)
```

With `depth_format` `0`, `slot_size` is `64 + stride * height` rounded up to a multiple of 64 and
bytes 40 to 63 of the file header are zero: the file is exactly what revisions 1 and 2 wrote. With
`depth_format` `1`, a writer sets `depth_stride = width * 4`, `depth_offset = 64 + stride * height`
rounded up to a multiple of 64, and `slot_size = depth_offset + depth_stride * height` rounded up
to a multiple of 64. (Receivers read the three values from the header and do not recompute them.)

**Depth plane.** `depth_format` `1` means one little-endian `f32` per pixel: the **linear view
depth in metres**, the distance of the surface along the camera's forward axis, that is `-z` in
view space (section 4), so it is positive in front of the camera. It is not the distance to the
eye, and it does not depend on the camera's projection, depth range or near and far planes, so a
guest and a host built on different engines read it the same way. `+inf`, or any other non-finite
value, means "nothing here". Rows run top to bottom like the colour, row `y` starts at
`depth_offset + y * depth_stride`, and only the first `width` values of a row, with `width` and
`height` from the slot header, are used; the rest is padding. Pixels whose alpha is 0 are empty
whatever their depth; a writer should still store `+inf` there. Colour and depth belong to one
frame: the seqlock below covers both, there is no second counter, and a writer with a depth plane
writes depth with every frame, so a slot never holds the colour of one frame and the depth of
another.

The `Camera` that the frame was rendered with is the one named by `camera_seq`. Section 9.7 says
how the depth is converted to and from a depth buffer and how the host reprojects and composites.

`lock` is 8-byte aligned in the file and must be accessed with atomic 64-bit operations.

Writer:
1. Store `lock + 1` (now odd), then issue a release fence
   (`std::atomic_thread_fence(std::memory_order_release)`).
2. Write the slot header fields, the pixels and, if the file has one, the depth plane.
3. Store `lock + 1` again (even), with release ordering.
4. Send the `Frame` datagram.

Reader, on a `Frame` datagram:
0. If `frame` is 0 (a datagram never carries it, section 9.6, but a caller can hand a reader one, and an
   unwritten or odd slot may hold frame 0) or `slot` is not below the file's `slots`, skip this frame. (The reader finds the slot by `slot`
   and does not require `slot == frame % slots`; a writer produces that, and step 3 is what ties the
   slot to the frame.)
1. `l1 = lock` (acquire). If `l1` is odd, skip this frame.
2. Read the slot header's `width` and `height`. They must each be at least 1 and at most the file's, and
   equal the datagram's `width` and `height`; if not, skip this frame, before anything is copied. Then
   copy the header fields, the pixels and the depth plane (if wanted) out.
3. Acquire fence, then `l2 = lock`. If `l2 != l1` or the copied `frame` is not the datagram's
   `frame`, the slot was overwritten while copying: discard the copy and wait for the next frame.
   (The overwritten case cannot be written as a static file, so no vector shows it; the live tests of
   each implementation do.)

With 3 or more slots the writer rarely reaches the slot being read.

A writer refuses a write whose width or height is 0 or above the file's, whose colour buffer is not
`width * height * 4` bytes, whose depth is not `width * height` values or is given for a file
without a depth plane. A refused write changes nothing: no byte of the file, no frame number used. A
writer whose file has a depth plane writes depth with every frame; a call that supplies none is
either refused or filled with `+inf` ("nothing here"), as the library decides, and a slot never
keeps the depth of an older frame.

**Header checks.** Every reader refuses a file whose `magic` is not `"RBFRAME"` and a zero byte,
whose `version` is not 1, whose `format` is not 1, or whose `slot_size` is not a multiple of 64 or is
below `64 + stride * height` (it has to hold the slot header and the colour rows). The field tables
above fix these values; revision 5 says in so many words that a reader refuses the others. The
`reserved` bytes of the file header (56 to 63) and of the slot header (36 to 63) are ignored on read
whatever they hold, as they are in datagrams.

**Compatibility and checks.** The depth fields use bytes that revisions 1 and 2 reserved, and
`slot_size` grows to make room, so older readers keep working: they read the header fields they
know, find slot `i` by `slot_size`, and read the colour as before, never looking at the reserved
bytes. A reader that wants depth checks, before using the file, that `depth_format` is `0` or `1`
and, for `1`, that `depth_stride` is `width * 4`, `depth_offset` is a multiple of 64 and at least
`64 + stride * height`, the plane `depth_offset + depth_stride * height` ends within `slot_size`, and
the file is at least `64 + slots * slot_size` bytes long (compute in 64 bits, or check for
overflow). A file that fails any of these, or any check of "Opening and creating" below, is
malformed and the reader refuses it. A `depth_format` the reader
does not know is not an error: the reader treats the file as colour only, so a later revision can add
a depth format without breaking older readers. With `depth_format` `0` or an unknown value the
reader ignores `depth_stride` and `depth_offset`. A reader that does not use depth at all may skip the depth checks, as a revision 2
reader does; the other checks are not optional.

**Opening and creating (revision 4).** The path in a `frame_channel` comes from the peer, and so
does every byte of the file. A receiver MUST:

1. refuse a `path` that is not absolute, and one that does not name a *regular file*: a symlink,
   FIFO, device, directory or socket is refused. It checks the path before opening it and the
   opened handle again (`fstat`; on Unix both must be the same device and inode). Where the OS has
   non-blocking opens (`O_NONBLOCK`) it opens the file that way, so a FIFO a peer names cannot
   block the receiver: opening one for reading waits for a writer for ever. (Java cannot ask for a
   non-blocking open; it relies on the check before the open, and the swap race remains.)
2. refuse a file whose `slots` is not 1 to 16, whose `width` or `height` is 0 or above **16384**,
   whose `width * height` is above **67108864** pixels (2^26, that is 8192 x 8192), or whose
   `stride` is not exactly `width * 4`, before it computes any size from them. The pixel limit
   caps what one read allocates at 256 MiB of colour plus 256 MiB of depth; a 16384 x 16384 file
   would have been 1 GiB of colour alone. A receiver that cannot afford even that may refuse
   files above a limit of its own, and a sender stays within both limits.
3. do all size and offset arithmetic with overflow checks: `64 + slots * slot_size`,
   `64 + slot * slot_size`, `64 + stride * height`, `depth_offset + depth_stride * height` and
   the sums in "Compatibility and checks" above. A header whose numbers overflow is malformed. (A
   `slot_size` chosen so that `slots * slot_size` wraps in 64 bits once passed the file-length
   check of a reader that multiplied unchecked.)
4. compare the file's current length (`fstat`, `FileChannel.size()`) with the length it mapped
   **before each read**, and treat a shorter file as gone: skip the frame, drop the mapping, wait
   for a new `frame_channel`. A file that shrinks under a live mapping makes the next access
   fault (SIGBUS), which ends the process. The check narrows that window but cannot close it: a
   truncation between the check and the copy still faults. The other half is on the sender (next
   paragraph); the check guards against a broken or hostile peer, not a correct one.

A writer that is asked to create a file with a number of slots outside 1 to 16, a width or height
of 0 or above 16384, or more than 67108864 pixels (the limits above) fails and creates nothing; one
asked for a path that exists fails and leaves what is there alone.

A sender MUST create the file exclusively (`O_CREAT | O_EXCL`, `CREATE_NEW`; creation fails if the
path exists, so it never opens over another file) and MUST NOT truncate, shrink or resize it, or
create another file at the path, after announcing it. A new size or a new session gets a new path
and a new `frame_channel`.

The sender deletes the file at the end of the session; a receiver never deletes it, and unmaps and
closes it when the session ends or a new `frame_channel` replaces it. On Windows:

- both sides open the file with `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`, so the
  receiver can open it while the sender has it, and the sender can delete it while the receiver
  still has it mapped (Windows removes the file once the last handle and view close);
- the sender unmaps its own view (`UnmapViewOfFile`, then closes the mapping and file handles)
  before deleting, since Windows refuses to delete or resize a file the calling process maps (a
  Java sender can unmap only through JDK internals, which it tries by reflection; where that is
  not possible the file outlives the writer on Windows until the temp directory is cleaned);
- a file cannot be resized while any process maps it, so a sender never reuses a path a receiver
  may still map: a new size or a new session gets a new file and a new `frame_channel`.

A sender that crashes leaves its file behind. Names should contain the sender's process id, so a
later run can tell its own files from a dead process's.

### 9.7 Depth, reprojection and compositing

This is how a receiver (normally the host) turns a guest's frame, colour and depth, into pixels of
its own image. The wire does not change: it uses the pixel file of section 9.6 and the `camera_seq`
that `Frame` already carries. The steps below are what a receiver **should** do; a receiver that
skips them still works, it just pastes the guest's image over its own, as before revision 3.

The Rust crate's `link::compose` module is a reference implementation on the CPU. It is meant for
testing and small images; an engine does the same on the GPU.

Conventions: pixel `(0, 0)` is the top-left corner and a pixel's centre is at `(x + 0.5, y + 0.5)`.
NDC has +x to the right and +y up (section 9.3), so `ndc_x = (x + 0.5) / width * 2 - 1` and
`ndc_y = 1 - (y + 0.5) / height * 2`.

**Depth buffer to linear metres.** A host has its scene depth in a depth buffer in the convention
of its `Camera.depth_range`, and needs it in linear metres to compare it with the guest's. Let `n`
be `near`, `f` be `far`, `d` the value in the convention of `depth_range` (section 9.3) and `t` the linear
depth in metres. For a perspective projection with `w_clip = -z_view`:

| `depth_range` | `d` of a surface `t` metres away | `t` from `d` | infinite `far`: `d` | infinite `far`: `t` |
|---|---|---|---|---|
| 0, near -1, far +1 | `(f+n)/(f-n) - 2fn/((f-n)t)` | `2fn / ((f+n) - d(f-n))` | `1 - 2n/t` | `2n/(1-d)` |
| 1, near 0, far 1 | `f(t-n) / (t(f-n))` | `fn / (f - d(f-n))` | `1 - n/t` | `n/(1-d)` |
| 2, near 1, far 0 | `n(f-t) / (t(f-n))` | `nf / (n + d(f-n))` | `n/t` | `n/d` |

For range 0, `d` is NDC `z`, in `-1 ..= 1`. An OpenGL depth buffer stores `(z + 1) / 2`, so convert
first (`d = 2 * stored - 1`). These formulas are the inverse of the projection's depth row, which
for a perspective projection built from `near` and `far` is `z_clip = a * z_view + b` with:

| `depth_range` | `a` | `b` | `a`, `b` with infinite `far` |
|---|---|---|---|
| 0 | `(f+n)/(n-f)` | `2fn/(n-f)` | `-1`, `-2n` |
| 1 | `f/(n-f)` | `fn/(n-f)` | `-1`, `-n` |
| 2 | `n/(f-n)` | `nf/(f-n)` | `0`, `n` |

(`d = z_clip / w_clip` and `w_clip = -z_view = t`.) A value that gives no positive finite `t`, a
cleared buffer for instance (`d = 0` in range 2, or `d = 1` with an infinite `far` in ranges 0 and
1), means "nothing here": `t = +inf`. Compute in a wider type than `f32` where you can, since
depth values close to 1 resolve distant surfaces coarsely in ranges 0 and 1. For an orthographic
projection depth is already linear in the buffer and the host converts it with its own matrix.

**Reprojection.** The guest renders with a `Camera` the host sent and puts that datagram's header
`seq` in `Frame.camera_seq`. The host keeps the cameras it sent in a small ring keyed by `seq`,
covering at least the last half second (32 cameras at 60 Hz). To present a frame it looks up `camera_seq` and reprojects the guest image from that
render camera to the camera it is presenting now, which is newer, so the guest's image is shifted by
the host's own motion in the meantime and only the guest's own animation shows the delay. For each
non-empty guest pixel, with linear depth `t`:

1. Unproject with the render camera: the view-space point is `(x_v, y_v, -t)`, where for a centred
   perspective projection `x_v = ndc_x * t / proj[0]` and `y_v = ndc_y * t / proj[5]` (for any
   other projection, solve the first two rows of `proj` for `x_v` and `y_v` given `w_clip` and
   `z_v = -t`). Then `p_world = inverse(view_render) * (x_v, y_v, -t, 1)`.
2. Project with the present camera: `p' = view_present * p_world`, `t' = -p'.z` (skip the pixel if
   `t' <= 0`), `clip = proj_present * p'`, `ndc = clip.xy / clip.w`, window position
   `((ndc_x + 1) / 2 * W, (1 - ndc_y) / 2 * H)`.
3. Write the colour and `t'` into the destination pixel (or pixels: a pixel's footprint may cover
   several), keeping the smaller `t'` where two land on one.

Destination pixels nothing landed on are **holes**: transparent, depth `+inf` (the guest never saw
what a disocclusion reveals, and the old view does not cover the whole new one). Anything that
depends on the viewpoint inside the guest's shading, such as reflections, is not corrected. If
`camera_seq` is 0, or not in the ring, the host composites without reprojection, as if the guest
had rendered with the current camera.

**Compositing.** For each pixel the host compares the guest's linear depth `t_g`, after
reprojection, with its own scene depth `t_h` converted to linear metres, and the nearer one wins:

- a guest pixel takes part if its alpha is above 0 and `t_g` is finite and positive;
- it covers the host's pixel if `t_g < t_h` (a non-finite `t_h` means the host drew nothing there);
  equal depths go to the host;
- where it covers, the result is the straight-alpha "over": `rgb = g * a + h * (1 - a)`, with the
  colours as the file carries them (sRGB-encoded; an engine may convert to linear light first);
- otherwise the host's pixel is kept.

This is occlusion both ways: a wall 5 m from the camera hides a guest object 8 m away, and a guest
object 3 m away covers the wall. A guest that sends no depth (`depth_format` `0`) is pasted over the
host's image, nothing can hide it and it cannot hide anything.

To tell how stale a frame is, compare the present time with the slot's `time_us` (section 5).

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
#define RBL_MAX_COLLISION_CHUNKS 4096  /* chunk_count at most; 4096 * 226 = 925696 triangles */

enum rbl_kind { RBL_HEARTBEAT = 1, RBL_CAMERA = 2, RBL_POSE = 3, RBL_COLLISION = 4, RBL_FRAME = 5 };
enum rbl_depth_range { RBL_DEPTH_NEG_ONE_TO_ONE = 0, RBL_DEPTH_ZERO_TO_ONE = 1, RBL_DEPTH_REVERSED = 2 };
enum rbl_depth_format { RBL_DEPTH_NONE = 0, RBL_DEPTH_LINEAR_F32 = 1 };  /* pixel file depth_format */
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
    uint32_t depth_format;       /* 40, rbl_depth_format; 0 in revision 1 and 2 files */
    uint32_t depth_stride;       /* 44, width * 4, or 0 */
    uint64_t depth_offset;       /* 48, from the slot start, multiple of 64, or 0 */
    uint8_t  reserved[8];        /* 56 */
} rbl_frame_file;

typedef struct rbl_frame_slot {  /* 64 bytes at 64 + i * slot_size; pixels follow, then
                                    the f32 depth plane at depth_offset, if the file has one */
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
- v1, revision 3: the pixel file can carry a depth plane next to the colour (section 9.6:
  `depth_format`, `depth_stride` and `depth_offset` in file header bytes 40 to 55, `slot_size`
  grown to cover the plane), and section 9.7 says how a receiver reprojects a frame with the
  `camera_seq` it already has and composites it with a depth test. Section 9.3 now says that
  `proj` is +y up in NDC. No message and no existing field changed, so the protocol version stays
  1: the new fields sit in bytes that revisions 1 and 2 reserved (written as zero, ignored on
  read) and zero keeps the old meaning, `slot_size` is what readers already use to find slots, so a
  revision 1 or 2 reader reads the colour of a depth file as before, and a file without depth is
  byte for byte what revision 2 wrote. A `depth_format` a reader does not know means colour only,
  so later depth formats (16-bit, half float) can be added the same way.

- v1, revision 4: validation and pacing, from a code review of both implementations. No byte
  layout, message or field changed. What changed:
  - **Frame numbering across pixel files (sections 7, 8, 9.6).** A new `frame_channel` resets the
    receiver's newest-frame number, because each file numbers from 1; before, a resize made every
    frame of the new file look stale until it passed the old file's count.
  - **Collision bursts (sections 2, 9.5).** Senders queue the chunks of a snapshot and send at
    most 32 collision datagrams per tick, a newer snapshot replacing queued chunks of an older one
    (SHOULD); receivers ask for a 4 MiB UDP receive buffer (SHOULD). A snapshot of 400 chunks, sent
    at once, mostly did not arrive.
  - **Opening and creating pixel files (section 9.6, MUST).** Receivers refuse a path that is not
    absolute or not a regular file (so a FIFO cannot hang them), refuse `width` or `height` above
    16384, a frame of more than 67108864 pixels (a sparse file whose header claimed 16384 x 16384
    made one read allocate 1 GiB) or a `stride` other than `width * 4` (the Rust reader had
    accepted larger), check all size
    arithmetic for overflow (the Java reader's `slots * slot_size` could wrap past the file-length
    check), and compare the file's length with their mapping before each read (a truncating peer
    killed the reader with SIGBUS). Senders create the file exclusively and never truncate or
    reuse a path (the Rust writer had truncated whatever was there).
  - **Entity tables (sections 8 `spawn`, 9.4).** Pose state exists only for entities the peer
    spawned, at most 1024 live ones; poses of other ids are ignored and counted. Before, every id a
    peer invented added an entry to a table that was never trimmed. This includes a host's poses of
    its own entities: they need a `spawn` too, as the table in section 1 already implied.
  - **`heartbeat_ms` (section 8 `hello`).** 10 to 10000; outside it the `hello` is a handshake
    error.

  Why the protocol version stays 1: every datagram and every control line a correct revision 3
  implementation sends is still valid and still means the same. The new rules reject only what
  the layout sections already called invalid (a `stride` other than `width * 4`, a file shorter than
  its header says, an entity id nobody spawned) or what no implementation produces (a heartbeat under
  10 ms, a frame over 16384 pixels on a side or over 8192 x 8192 in all, 1025 live entities: the
  Java writer already stopped at 32768 and the defaults are 250 ms and a handful of entities). Pacing and the buffer size are the
  sender's and receiver's own behaviour, invisible on the wire. A peer that depended on the old
  leniency (poses without a `spawn`, or a `heartbeat_ms` under 10) must add the `spawn` or raise
  the value; the reference examples (`fake_guest`, link-java's `LinkDemo`) already spawn first.

- v1, revision 5: the places where revision 4 was silent or ambiguous, settled. `tests/vectors/link/GAPS.md`
  lists all 26, with what each of the four implementations (Rust, link-java, link-c, link-cs) did before, the
  rule, and the reason; the shared vectors now cover the rules. No byte layout, message or field changed.
  Where the four implementations already agreed the rule is what they did, written down and tested; where
  they differed, or all four accepted without a limit what no receiver can afford, the rule is the safest
  one. What the text now says:
  - **First line, second `hello`, `bye` (sections 6, 8).** The first line is a `hello` or a `bye`; any other
    line, known type or not, is a protocol error. A second `hello` is a protocol error. A `bye` needs no
    `reason`; `null` is absent; a reason of another type is a protocol error; the text of a reason is free
    (only `"busy"` is fixed). A `despawn` of an id that is not live is ignored.
  - **Syntax of a control line (section 8).** Duplicate keys: the last wins, and the overridden values must still
    be valid. Integer fields take digit-only literals (no `7.0`, `7e0`, `-0`). `null` for an optional field is
    absent. A CR inside the line is whitespace; a byte order mark, form feed, vertical tab and U+00A0 are not.
    Lone surrogate escapes are errors. **A `\u0000` escape is an error**, and a sender writes U+FFFD in its
    place. Arrays and objects nest at most 127 deep.
  - **Integer ranges of `hello` (section 8).** `protocol` and `min_protocol` are `u16`; `heartbeat_ms` and
    `timeout_ms` are `u32`; `udp_port` is 1 to 65535 (0 is a handshake error); `timeout_ms` has no range beyond
    its type because no receiver uses the peer's value.
  - **Datagrams (sections 2, 9).** A receiver's buffer is at least 8193 bytes; a datagram of a known kind above
    8192 bytes is malformed, one of an unknown kind is ignored at any length. A `Frame` with `frame` 0 is
    malformed. The decoder checks no value of a `Camera`, `Pose` or `Collision` beyond the layout ranges it
    already had; unknown `Pose` flag bits are ignored; receivers check before they use.
  - **Collision (section 9.5).** `chunk_count` is at most **4096** (925696 triangles, 32 MiB); the assembly
    rules (first chunk decides `chunk_count`, `center` and `radius`; a chunk with another `chunk_count` is
    dropped; the first copy of a chunk wins) are written out.
  - **Pixel file (section 9.6).** The reader's steps now say: frame not 0, slot below `slots`, the slot header's size 1 to the
    file's and equal to the datagram's, then the seqlock; `slot == frame % slots` is not required. The header
    checks (`magic`, `version`, `format`, `slot_size`) and the constructor limits are stated as MUST, reserved
    bytes are ignored, and a writer refuses a frame of size 0 or above the file's, or a buffer of the wrong
    length, and changes nothing.

  Why the protocol version stays 1: no datagram and no control line that a known sender writes becomes
  invalid, and none changes meaning. Three new rules do refuse something that the revision 4 text allowed
  and that a receiver let through, so this is a judgment about senders and not a proof:
  - a `\u0000` escape in a string. Revision 4 allowed it. The C implementation could not hold it (it cut the
    string at the NUL), and the others could only hand it on to code that could not either; no known sender
    writes one, and the new libraries write U+FFFD in its place.
  - a collision `chunk_count` above 4096. Revision 4 allowed up to 65535. No known sender comes near it
    (sending 4096 chunks takes over two seconds of pacing at 60 ticks per second, and the C and C#
    implementations already dropped what exceeded 2097152 triangles).
  - a `Frame` datagram whose size differs from the slot header's, or whose slot header has a size of 0.
    Revision 4 never said a slot header may not have size 0 (its table only says "pixels used, `<=` file
    width"). Every known writer writes the same size in both places and at least 1 x 1.

  A peer that depended on the old leniency has to write U+FFFD (or nothing) in place of NUL, keep a
  snapshot under 925696 triangles, and announce the size it wrote. link-java, link-c and link-cs add a
  call that takes the whole `Frame` datagram for the reader. The old size-less call (`read(frame, slot,
  wantDepth)` and its C and C# counterparts) is kept so that existing programs still build; **it does not
  implement reader step 2 above and is not conformant**: new code uses the other.

- v1, revision 6: an optional authentication token (section 6.2). What changed:
  - **`hello` (section 8).** A new optional field `token`, a string, sent only by a guest. It parses as any
    string (another type is a protocol error, `null` is absent) and is valid from 1 to 256 bytes.
  - **Handshake (sections 6, 6.2).** A host may be configured with a token. Such a host checks the
    guest's `token` before anything else, compares it in constant time, and refuses a missing, out of
    range or unequal one with `{"type":"bye","reason":"unauthorized"}`. A host without a token ignores the
    field. Nobody echoes or logs a token.
  - **`bye` (section 8).** `"unauthorized"` is the second fixed reason, after `"busy"`.
  - **Endpoint file (section 6.2).** `{"control_port":...,"token":"..."}`, written by the host after it has
    bound the port, atomically and owner-only, removed on a clean shutdown, read by the guest. Recommended
    places per OS, and what the token protects against and what it does not. A host that makes its own
    token takes 32 bytes from the OS's cryptographic generator and writes them as 64 lowercase hex digits.
  - **UDP (section 6.2).** Unchanged; the text says why authenticating the control connection is enough.

  Why the protocol version stays 1: no layout changes, and a field is added to a message, which section 8
  ("unknown fields are ignored") and the end of this section have always allowed. A host without a token (the default, and every host before revision
  6) behaves exactly as before. A revision 5 host ignores `token` as an unknown field, so a revision 6 guest
  with a token can still talk to it. A revision 5 guest cannot open a session with a host that has a token,
  which is the purpose: the host's owner turned authentication on. One thing a revision 5 receiver accepted is
  now refused: a `hello` whose `token` is not a string (revision 5 ignored it as an unknown field of any
  type). No known sender writes a `token` field other than a revision 6 guest, which writes a string.

A change that alters any layout or meaning above needs a new protocol version. Adding a control
message type or field, or a new datagram `kind`, does not, because receivers ignore what they do
not know. Neither does giving a meaning to `reserved` bytes, as revision 3 does, as long as zero
keeps the old meaning.
