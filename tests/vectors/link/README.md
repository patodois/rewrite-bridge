# Link protocol test vectors

Test vectors for the passthrough link protocol of `docs/CONTRACT.md` (protocol v1, revision 4). Every
implementation of the link (Rust in this repository, `link-java`, `link-c`, `link-cs`) reads these files in its
own test suite and must agree with all of them. They are derived from the contract alone: `gen.py` writes them
with an encoder of its own (standard library only), so no implementation's bug can have leaked into them. Where
the contract is silent there is no vector; `GAPS.md` lists those places.

Regenerate with `python3 tests/vectors/link/gen.py`, check that the committed files are current with
`python3 tests/vectors/link/gen.py --check`.

## Files

| File | What it holds |
|---|---|
| `control.json` | one control line each: what an implementation must decode, ignore or reject, and what it must encode |
| `datagrams.json` | one UDP datagram each, as hex: the same |
| `pixelfiles.json`, `pixelfiles/*.bin` | pixel files to open and read, files that must be refused, and the bytes a writer must produce |
| `sessions.json` | scripted sessions over real sockets against one side of the link (host or guest) |
| `SHA256SUMS` | `sha256sum` format, every file of this directory except itself and `gen.py` |
| `GAPS.md` | what the contract does not say |

Each JSON file is `{"format": "rbl-link-vectors/1", "kind": ..., "contract": ..., "cases": [...]}`. Every case has a
unique `id` (`ctl.*`, `dg.*`, `pf.*`/`pw.*`, `sess.*`) and a `section` of the contract it comes from. Some have a
`basis`: `MUST` (the contract says it in so many words, the default), `table` (the contract fixes the value in a
field table but does not say a reader must refuse anything else) or `derived` (follows from the text by reading it
closely). A runner reports a failing `table` or `derived` vector like any other, and the report says its basis.
`note` and `why` are for humans. Everything is ASCII; the JSON is one case per line.

Numbers in JSON: `u32` and smaller are numbers; `u64` (`time_us`, `frame`, `lock`) are decimal strings, so that no
JSON library has to hold 64-bit integers; `f32` values are numbers that are exact in binary32, or the string
`"inf"` for +infinity.

## Checksums and how the vectors reach the other repositories

`link-java`, `link-c` and `link-cs` keep a **copy** of this directory (without `gen.py`) at `tests/vectors/link/`.
`scripts/sync-link-vectors.sh <repo>` makes the copy and prints the digest to pin. Each repository's runner
(a) checks every file against `SHA256SUMS`, and (b) checks that the SHA-256 of `SHA256SUMS` itself equals a constant
in the runner's source (`VECTORS_DIGEST`), so a vector can only change together with a deliberate edit of that
constant. The interop workflow of this repository checks out the other repositories at pinned revisions and
fails if their copy differs from this directory. The copied files must not be changed by line-ending
conversion: each repository has a `.gitattributes` entry `tests/vectors/link/** -text`.

## `control.json`

A case has a `line` (the JSON text, without its line terminator) or `line_hex` (raw bytes, for invalid UTF-8),
and `expect`:

* `message`: the line must parse as the control message `message`, an object with the wire field names of the
  fields the contract defines (unknown fields on the line are ignored and are not listed). Absent optional fields
  are absent in `message`.
* `unknown`: the line is a JSON object with a string `type` the implementation does not know (`type` in the
  case); it parses without error and is ignored.
* `reject`: the line must be refused (a protocol error).

With `"encode": true` the implementation must also produce the line from `message`: its output, parsed as JSON,
must equal `message` exactly (key order, spacing and escapes are free; no extra keys, no `null` for an absent
optional field), and it must be a single line ending in `\n`.

## `datagrams.json`

`hex` is the whole datagram. `expect`:

* `decode`: the datagram is valid; `header` and `body` are the decoded values. Compare only the `header` keys that
  are present. `body.kind` is `heartbeat`, `camera`, `pose`, `collision` or `frame`, with the field names of the
  contract's section 10 (`near` and `far` for the C `near_m` and `far_m`). `pose` records and `collision`
  `triangles` (nine floats each: a, b, c) are listed in full. With `"encode": true` the implementation, given
  `header` and `body`, must produce exactly `hex`.
* `ignore`: a datagram of unknown `kind` (the case's `kind`, and `payload_len`): decoding succeeds and reports an
  unknown kind, or at least is not an error.
* `reject`: decoding fails.

## `pixelfiles.json`

Reader cases have `file` (a path under this directory) or `recipe` (`header_hex` written at offset 0 of a new file,
then the file is set to `length` bytes, zero filled and sparse where the filesystem allows) or `path_kind`. The
runner copies a `file` to a fresh temporary file (never opens the fixture itself) and opens it as the receiver of
a `frame_channel` would. `open` is `ok` (with `info`: `slots`, `width`, `height`, `stride`, `slot_size`,
`has_depth`) or `refused`. `path_kind` cases check the path rules of the contract: `relative` (a relative path
naming a valid copy of `file`), `missing`, `empty`, `directory`, `symlink` (to a valid copy of `file`), `fifo`;
a runner skips a kind it cannot create on its platform (symlink and FIFO on Windows). Cases with `"large": true`
need a sparse file of up to 1 GiB; the environment variable `RBL_VECTORS_SKIP_LARGE=1` skips them. `skip_on`
lists platforms (`windows`) where the case is skipped.

Each entry of `reads` is a `Frame` datagram (`frame`, `slot`, `camera_seq`, `width`, `height`) followed by what
the read must give: `expect: "ok"` with the slot header values (`frame`, `time_us`, `camera_seq`, `width`,
`height`), the colour bytes `rgba` (hex, tightly packed `width * height * 4`; or `rgba_sha256` when `rgba` is
null) and `depth` (a list of `width * height` floats, row-major, or `null` when no depth is wanted or the file has
none; `want_depth` says whether the reader is asked for it), or `expect: "skip"`: the frame must not be
delivered (an odd lock, another frame in the slot, a slot that does not exist, a file that has shrunk). A
`then` list continues the case: `{"op": "truncate", "length": N}` truncates the file the reader holds open, and
`{"op": "read", ...}` is another read.

Writer cases (`pw.*`) have `writer` (`width`, `height`, `slots`, `with_depth`, and `writes`: the frames to write
in order, each with `camera_seq`, `time_us`, `width`, `height`, `rgba` and `depth`). The writer is created at a
new path in the temporary directory, the frames are written, and the file's bytes must equal `file` exactly.
`expect: "create_fails"` means creating the writer must fail and leave no file; `create_fails_and_file_untouched`
creates it at a path that already holds a file, which must stay as it was.

## `sessions.json`

A session case runs one **implementation under test** (UT), host or guest (`role_under_test`), against a raw
peer driven by the runner with plain sockets. `config` overrides the UT's `heartbeat_ms` (default 250),
`timeout_ms` (default 2000) and `reconnect_ms` (guest, default 200). The UT is polled continuously (every few
milliseconds) while the runner waits for anything; its events are queued. Cases with `"slow": true` take seconds.

Set-up: for a host UT the runner starts `Link::host` on control port 0 and UDP port 0; for a guest UT it opens a
TCP listener and a UDP socket of its own on 127.0.0.1, port 0, and starts the guest pointing at the listener.
Both sides use 127.0.0.1 only. The raw peer has one UDP socket, whose port it announces as `udp_port`.

Variables, substituted into the strings of `send_line` (`text`), `send_range` (`template`) and into the hex of
`send_datagram`: `${peer_udp_port}`, `${ut_udp_port}` and `${session}` (decimal), `${i}` (in `send_range`).
`${session}` and `${ut_udp_port}` are set by `capture` (below) or `set`. In datagram hex, `{session_le}`,
`{session_le_plus1}` and `{session_le_minus1}` are the session as 8 hex digits, little-endian (plus or minus one,
modulo 2^32).

Steps, in order. Waits take up to 3 seconds unless a step says otherwise; a step that times out fails the case.

| `op` | Fields | Meaning |
|---|---|---|
| `connect` | `conn` (default `a`) | host UT: the raw peer opens a TCP connection to the UT's control port |
| `accept` | `conn` | guest UT: the raw peer accepts the UT's connection on its listener |
| `send_line` | `text`, `conn` | write `text` as UTF-8 |
| `send_hex` | `hex`, `conn` | write raw bytes |
| `send_fill` | `prefix`, `fill`, `count`, `suffix`, `conn` | write `prefix`, `fill` repeated `count` times, `suffix` (ASCII) |
| `send_range` | `template`, `from`, `to`, `conn` | write `template` for `i` = `from` ..= `to`, in one write |
| `expect_line` | `match`, `capture`, `exact`, `conn` | read the next line from the UT; it must be a JSON object that has every key of `match` with that value (extra keys are fine, unless `exact` is true: then the key sets must be equal). A string value starting with `$` is a pattern: `$string`, `$int` (integer >= 0), `$u32nz` (1 ..= 4294967295), `$u16nz` (1 ..= 65535), `$hb` (10 ..= 10000). `capture` maps variable names to line keys: `{"session": "session"}` stores the line's `session` as `${session}`, `{"ut_udp_port": "udp_port"}` stores `udp_port` as `${ut_udp_port}` |
| `expect_no_line` | `ms` | no complete line arrives for `ms` milliseconds and the connection stays open |
| `expect_closed` | `allow_lines`, `within_ms` (default 3000), `conn` | the UT's end of the connection reaches end of stream within `within_ms`. Any complete line read before it is a failure unless `allow_lines` |
| `close` | `half`, `conn` | the raw peer closes the connection (`half`: only shuts down its write side) |
| `set` | `vars` | set variables |
| `sleep` | `ms` | keep polling the UT for `ms` milliseconds |
| `send_datagram` | `hex`, `from` | the raw peer's UDP socket sends the datagram to the UT's UDP port (`from: "other"`: from a second socket whose port was not announced) |
| `expect_datagram` | `kind`, `count` (default 1), `timeout_ms`, `like`, `seq_consecutive` | datagrams from the UT's UDP port must arrive, each with the magic, version 1 and the captured session. `kind`: only datagrams of that kind count (others are skipped); `like`: the `id` of a `datagrams.json` case: the datagram must equal that case's `hex` except for the session, `seq` and `time_us` header fields, and its kind is the case's. `seq_consecutive`: the counted datagrams' `seq` values rise by exactly 1 |
| `expect_no_datagram` | `ms` | nothing arrives from the UT's UDP port for `ms` milliseconds |
| `ut` | `do`, plus fields | the UT sends: `spawn` (`entity`, `provider`, `asset`, `name` or null), `despawn` (`entity`), `set_focus` (`focus`), `announce_frames` (`path`, `width`, `height`, `slots`), `disconnect` (`reason`), and `send_camera`, `send_poses`, `send_frame` with `like`: the `body` of that `datagrams.json` case is passed to the UT's send call |
| `expect_focus` | `focus` | the UT's own idea of who has the input focus (`host` or `guest`), as its API reports it, equals `focus` right now (after polling) |
| `expect_event` | `event`, `match`, `count` (default 1) | take the next `count` events of that type from the UT's queue (waiting for them); each must match. Events of other types stay queued |
| `expect_no_event` | `event`, `ms` | after `ms` milliseconds of polling the queue holds no event of that type |

Event types and the keys a `match` may use: `connected` (`role` and `game` of the peer, `protocol`, `session`),
`lost`, `handshake_failed` (no keys: the reason text is not specified), `camera` (`seq` of the datagram header,
`viewport_width`, `viewport_height`, `depth_range`), `poses` (`count`, `entities`: the entity ids in order),
`collision` (`query`, `triangles`: the total number of triangles), `frame` (`frame` as a decimal string, `slot`,
`camera_seq`, `width`, `height`), `spawn` (`entity`, `provider`, `asset`, `name`), `despawn` (`entity`), `focus`
(`focus`: `host` or `guest`, `requested`: whether this is a request, which is what a host sees), `frame_channel`
(`path`, `width`, `height`, `slots`).

A session case ends by closing the UT (and its sockets); the next case starts from nothing. Events in the queue
at the end of a case are ignored.
