# Cross-language interop of the link

Every host/guest pair among the four implementations of the link (Rust in `examples/interop_peer.rs`, Java in
`link-java`, C in `link-c`, C# in `link-cs`) runs the same scripted session over real sockets and a real pixel
file, 16 pairs in all, each in three modes (below): 48 runs. `run.sh` runs the matrix; `check_results.py` judges the two result lines of each pair.
`.github/workflows/link-interop.yml` runs it on Linux with the other repositories checked out at the revisions
pinned in `pins.txt`.

## The peer program

Each implementation provides a program, `interop_peer`, with one command line:

    interop_peer host  --port PORT --label LABEL [--endpoint FILE [--expect-unauthorized]]
    interop_peer guest --port PORT --label LABEL [--endpoint FILE [--wrong-token]]

`host` listens on control port `PORT` (UDP port 0). `guest` connects to `PORT` (UDP port 0), retrying until a
host answers. With `--endpoint` (contract revision 6, section 6.2) the host has a token, which its library generates,
and the library writes `FILE` after binding (`PORT` is 0 then, so the file is the only place the port can be read
from) and removes it when the host closes its link; the guest gives its library `FILE` as its endpoint file, so the
library reads the port and the token from it before connecting, and ignores `PORT`. See "Modes". Both use heartbeat 100 ms and timeout 3000 ms, and the game name `interop host (LABEL)` or
`interop guest (LABEL)`. A peer gives up, and fails, 30 seconds after it started or when a step below waits more
than 10 seconds. It prints log lines of its own choosing, and as its **last stdout line** exactly one line

    RBL-INTEROP-RESULT {json}

and exits 0 if every check it made passed, 1 otherwise, 2 for a bad command line. The JSON object has these
keys (a count is an integer; `stats` is `null` when the library has no counters):

| Key | Meaning |
| `presentation_rejections` | Host: 9 validated presentation rejections, one duplicate per round and four older slots; raw reads prove those slots remain readable. Guest: 0. |
|---|---|
| `role`, `label` | as given |
| `ok` | `true` if no check failed |
| `failures` | list of strings, one per failed check (empty when `ok`) |
| `peer_game` | the peer's game name from its hello (`interop guest (LABEL)` or `interop host (LABEL)` with the *other* side's label; a peer cannot know that label, so it only checks the shape, and `check_results.py` checks the label) |
| `protocol` | the negotiated version from `Connected` |
| `session` | the session number from `Connected`, a JSON number |
| `spawns` | entity ids of the peer's `spawn` events, in order |
| `despawns` | entity ids of the peer's `despawn` events, in order |
| `cameras` | `Camera` events received and verified |
| `poses` | `Poses` events received and verified |
| `collision_triangles` | triangles of the assembled collision snapshot (0 if none) |
| `frames_ok` | frames read from the peer's pixel file and verified (host only, else 0) |
| `frames_bad` | frames announced but missing or wrong |
| `focus` | focus events seen, in order, as strings `"<focus>:<kind>"`: `<focus>` is the `focus` value the event carries (`host` or `guest`), `<kind>` is `request` when the event says the peer asked for it (what a host sees) and `set` when the peer set it (what a guest sees) |
| `lost_reason` | the reason text of the `Lost` event, or `null` |
| `mode` | `plain` (no `--endpoint`), `token` (`--endpoint`) or `wrong-token` (`--expect-unauthorized`, `--wrong-token`) |
| `connected` | whether a `Connected` event was seen |
| `handshake_failed` | the reason texts of the `HandshakeFailed` events, in order |
| `stats` | `{"malformed","foreign","stale","unknown_entity","spawn_ignored"}` from the library's counters, or `null`. Read when the rounds are over: by the host just before it sends its `despawn` of 3, by the guest on seeing that despawn. (Later, a heartbeat of the other side still in flight when the session ends is legitimately "foreign" to a link that is no longer in a session.) `check_results.py` expects every counter to be 0, except that a guest may count one `foreign`: the host sends a `Heartbeat` immediately after its `hello` line, on another socket, and the guest can read that datagram before the line, when it has no session yet. |

## The session

Both sides poll their link continuously (every few milliseconds). Constants are exact; floats are exact in
binary32. `N = 5`. The host's entity is 3 (provider `War Thunder`, asset `/interop/tank`, name `Host Tank`), the
guest's is 7 (provider `Warframe`, asset `/Lotus/Characters/Excalibur`, name `Excalibur`).

1. **Handshake.** Both wait for `Connected`. Each checks `peer_game` is the other's game name and `protocol` is 1.
   The two `session` values must be equal and nonzero (the driver compares them).
2. **Spawn and focus.** On `Connected` the host spawns entity 3 and the guest spawns entity 7 and creates a pixel
   file with `FRAME_W = 64`, `FRAME_H = 48`, 3 slots and a depth plane (in the temp directory, created
   exclusively, named with its process id), and announces it with `frame_channel`. The host then gives the
   focus to the guest (`input` `guest`); the guest, on seeing it, requests it back (`input` `host`); the host,
   on seeing the request, sets the focus to `host`; the guest sees that too. Focus events thus read, host side:
   `["host:request"]` (the guest asked for `host`), guest side: `["guest:set", "host:set"]`.
3. **Setup done.** The host waits for the guest's `spawn` of 7 and its `frame_channel` and opens the pixel file
   (checking that the announced `width`, `height` and `slots` are 64, 48, 3); the guest waits for the host's
   `spawn` of 3. Then the host starts step 4.
4. **Frames.** For `k` = 1 ..= N, lock-step:
   * The host sends `Camera k`, remembers the `seq` it was sent with, and sends `Pose k` for entity 3. For
     `k` = 1 it also sends a collision snapshot of 300 triangles (two chunks).
   * The guest, on `Camera` event `k` (it counts them), checks every field against `Camera k`, then sends `Pose k`
     for entity 7, writes frame `k` (colour and depth, `camera_seq` = the `seq` of that camera's datagram header,
     `time_us` = `1_700_000_000_000_000 + 16_666 * k`) into its pixel file and sends the `Frame` datagram. It
     also checks the host's `Pose k` (an event of entity 3) and, for `k` = 1, the collision snapshot.
   * The host waits for the guest's `Poses` event (entity 7) and `Frame` event, checks the pose against
     `Pose k` and reads the frame from the pixel file with depth: `camera_seq` must be the `seq` it remembered,
     width 64, height 48, `time_us` as above, the colour and depth must be exactly `Pixels k`. Only then it
     moves to `k + 1`.
5. **Despawn.** The host, once it has verified frame `N`, sends `despawn` of 3. The guest, on seeing it, sends
   `despawn` of 7 and the host waits for it. (This order matters: control lines are read before datagrams in a
   poll, so a guest that despawned right after its last `Frame` could have the despawn processed before that
   frame's pose, which would then count as `unknown_entity`. The host's despawn tells the guest that the host has
   read everything the guest sent.)
6. **Bye.** The guest, once it has seen the host's despawn and sent its own, calls `disconnect("done")`, keeps
   polling for 200 ms so the closing drains, and closes its link. The host waits for `Lost`; its reason must
   contain `bye` (all four libraries say `peer said bye: done`). The guest must not get a `Lost` event. Both
   print their result.

### Data

* `Camera k`: `view` = identity with translation `(k, 0.5, -8)`, i.e. the sixteen floats
  `1,0,0,0, 0,1,0,0, 0,0,1,0, k,0.5,-8,1`; `proj` = `1.5,0,0,0, 0,2,0,0, 0,0,-1,-1, 0,0,-0.25,0`; `fov_y` 1.0;
  `near` 0.25; `far` = `+inf` for odd `k`, 500 for even `k`; viewport 64 x 48; `depth_range` = `k mod 3`.
* `Pose k` of the host's entity 3: `flags` = 1 (`TELEPORT`) for `k` = 1, else 0; `time_us` =
  `1_700_000_000_000_000 + 1000 k`; `translation` `(k, 2, -3)`; `rotation` `(0, 0, 0, 1)`; `scale` `(1, 1, 1)`;
  `velocity` `(0.5, 0, 0)`; `angular_velocity` `(0, 0.25, 0)`. Of the guest's entity 7: the same with `time_us` =
  `1_700_000_000_000_000 + 1000 k + 500` and `translation` `(-k, 1, 4)`.
* Collision snapshot: `center` `(1, 2, 3)`, `radius` 50, triangle `i` (0 <= i < 300) = vertices
  `(b, 0, 0), (b + 1, 0, 0), (b, 0, 1)` with `b = i mod 64`. The guest checks the count, and all coordinates.
* `Pixels k` (`W = 64`, `H = 48`, `i` = byte index): `rgba[i] = (37 i + 11 k + 5) mod 256`, except that pixel 0
  (bytes 0 to 3) is `0,0,0,0`. `depth[p]` for pixel `p`: `+inf` for `p` = 0, else `0.5 (p + 1) + k`.

## Modes

* `plain`: no token, as before revision 6.
* `token`: the host runs with `--port 0 --endpoint FILE`, the guest with `--endpoint FILE`. The session is the one
  above, with the same checks. The driver also checks that the file has mode 0600 while the host runs, that it is
  gone after the host exits, and that neither peer's output contains the token.
* `wrong-token`: the host runs with `--port 0 --endpoint FILE --expect-unauthorized`, the guest with
  `--endpoint FILE --wrong-token`. The guest reads `FILE` with its library's reader and connects to its port with the
  file's token with the last character replaced (`0` by `1`, anything else by `0`), without giving its library the
  file. The host's link answers with `bye` `unauthorized`. The guest waits for its `HandshakeFailed` event, checks that
  the reason contains `unauthorized` and that no `Connected` came, and closes its link at once (before its library
  would reconnect). The host waits for a `HandshakeFailed` event, checks that its reason contains `unauthorized`,
  polls 300 ms more, checks that no `Connected` came, and closes its link. Both print a result line with `ok`,
  `failures`, `mode`, `connected` (false), `handshake_failed` and the other keys at their empty values (counts 0,
  lists empty, `peer_game`, `protocol`, `session` and `lost_reason` null, `stats` null or the counters). The driver
  checks the file as in `token` mode, and that neither peer's output contains the right token or the wrong one.

## The driver

    tests/interop/run.sh [--build] [--modes plain,token,wrong-token] [HOST_IMPL:GUEST_IMPL ...]

`HOST_IMPL` and `GUEST_IMPL` are `rust`, `java`, `c`, `cs`; without arguments all 16 pairs run, in every mode named
by `--modes` (default: all three). Where the
peers are:

* `--build` builds them first: `cargo build --release --features link --example interop_peer` here, and in the
  repositories named by `RBL_LINK_JAVA_DIR`, `RBL_LINK_C_DIR`, `RBL_LINK_CS_DIR` (default: siblings of this
  repository, `../link-java` and so on): Gradle's `testClasses` (Java 8 bytecode, a JDK 9+ is needed to build), `make build/interop_peer`,
  `dotnet build -c Release`.
* or set `RBL_PEER_RUST`, `RBL_PEER_JAVA`, `RBL_PEER_C`, `RBL_PEER_CS` to the full command that starts a peer
  (the driver appends `host|guest --port ... --label ...`), for example
  `RBL_PEER_JAVA="java -cp link-java/build/classes/java/main:link-java/build/classes/java/test dev.patodois.rewritelink.InteropPeer"`.

Each plain run gets its own port from 47810 (the other modes use port 0), and its logs in `$RBL_LOG_DIR` (default: a
new temporary directory, printed). The endpoint files go to a private temporary directory of their own, never into
the log directory. A pair passes when both peers exit 0 and `check_results.py` accepts their result lines: the cross
checks (equal sessions, the expected counts of each side, `lost_reason` containing `bye`, every `stats` counter
that exists zero) are in that script.

## Publishing the pins

Only published commits may appear in `pins.txt`. This branch deliberately keeps the existing
published pins until the 0.3.0 ports with the revision 7 vectors and presentation-aware peers
are published. Publish link-c, link-cs and link-java first; then update all three pins together
and release rewrite-bridge and rewrite-bridge-bevy 0.4.0. Local interop uses explicit peer paths.
Do not replace a pin with an unpublished local hash.
