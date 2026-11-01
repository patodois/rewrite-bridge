# Cloud report: coverage-guided fuzzing of `rewrite-bridge`

Branch `claude/fuzz-rewrite-bridge-02ez0h`. Toolchain: nightly 1.101 (2026-10-27) with cargo-fuzz 0.13.2, for the fuzz crate only.

## Result

**No panic, arithmetic overflow, out-of-memory, excessive allocation, hang or wrong-token acceptance was found.**
So no library code was changed and no regression test was added. Nothing was invented to fill that gap.
The only edit outside `fuzz/` is `Cargo.toml`: `exclude` gains `"fuzz/"`, so the fuzz crate stays out of the published package.

## What was added

`fuzz/` is its own workspace (`[workspace]` in `fuzz/Cargo.toml`), built with `cargo +nightly fuzz`.
It is not a member of the root workspace, is not in the published package, and CI does not run it.
It builds with `overflow-checks = true`, so arithmetic overflow is caught.

| target | entry point | checked beyond "does not panic" |
|---|---|---|
| `decode_datagram` | `decode_header`, `decode_datagram` | header and datagram agree; encode(decode(x)) is stable and at most 8192 bytes |
| `control_parse` | `Control::parse` | a parsed message re-serialises to one line that parses and serialises identically |
| `endpoint_parse` | `Endpoint::parse` | port is nonzero, token is 1..=256 bytes without NUL, input at most 4096 bytes |
| `frame_reader` | `FrameReader::open`, `read`, `read_depth` on arbitrary file bytes and an arbitrary `FrameReady` | output never larger than the file |
| `handshake` | `Link` as host, with and without token, and as guest, driven over loopback TCP and UDP by a raw peer | a token-protected host never reports `Connected` unless the exact token was shown; pose and collision event sizes stay bounded; the guest's hello carries the token only when configured |
| `reproject` | `compose::reproject`, `composite`, `linearize_*`, `delinearize_depth` with arbitrary cameras and small images | none |

`handshake` takes a small op-stream format, described in its header comment: raw control bytes, raw UDP, UDP patched to the live session, a well-formed hello with a chosen token mode, and a poll.
`Link` has no byte-level entry point for the control channel, so the target uses real sockets on 127.0.0.1.

`fuzz/seeds.py` regenerates the initial corpora from the repository's own synthetic vectors in `tests/vectors/link` and a few hand-built sessions.
No game files or game data were used.
`fuzz/corpus/` holds the corpora after `cargo fuzz cmin` (about 1.1 MB of content; `du` shows more because of small files).
`fuzz/artifacts/` is git-ignored and was empty at the end.

## Fuzzing time per target

All runs used `-timeout=10 -rss_limit_mb=2048`.

| target | wall time | executions |
|---|---|---|
| `decode_datagram` | 35 min | 112,088,634 |
| `control_parse` | 35 min | 19,523,578 |
| `endpoint_parse` | 35 min | 31,012,053 |
| `frame_reader` | 35 min | 43,980,168 |
| `reproject` | 35 min | 20,861,354 |
| `handshake` | 35 min + about 13 min of an earlier run | 212,343 in the 35-minute run (about 100 per second, because of real sockets and sleeps) |

Each run was started once, concurrently on 4 cores. No target kept finding things, so none was extended.
No finding appeared in any run, so none was extended past 35 minutes.

## Things that looked like findings but were not

- **Handshake harness error.** The first smoke run failed because my harness read the guest's hello without polling the link, so the hello was never flushed. I fixed the harness. Two `crash-*` files from that smoke run did not reproduce; I deleted them.
- **LeakSanitizer report.** The first long `handshake` run ended with a 32-byte leak. The stack was std's thread-local hook on the library's `rewrite-bridge-connect` helper thread (`CONNECT_TIMEOUT`) still running when libFuzzer stopped. It is not library data. I restarted that target with `-detect_leaks=0` and it ran its 35 minutes clean.

## Not done, and uncertain

- **Coverage depth.** `handshake` reached about 2100 coverage edges in 35 minutes at 100 executions per second. Longer runs or running several jobs in parallel could reach more. UDP is only delivered after a successful hello, and the fuzzer only learned the hello shape through the template op I gave it.
- **Not fuzzed.** The Bevy plugin, `Endpoint::read` and `FrameReader::open`'s path checks (symlink, FIFO), because they depend on the filesystem rather than on peer bytes. `Collision::chunks` and the encoders are not given peer bytes. `FrameWriter` is not a decoder.
- **Remaining risks.**
  - `FrameReader::read` ends with a documented race: a peer that truncates a mapped file can cause SIGBUS. This is in the type's docs and cannot be fuzzed from within one process.
  - A reassembled collision snapshot can reach about 33 MB (4096 chunks of 226 triangles). That is bounded by 4096 real datagrams, so I judged it justified by the input and left it.
- **Pre-existing clippy lint.** `cargo clippy --all-targets --features link -- -D warnings` fails with clippy 1.97 on `src/conformance/animation.rs:357` (`manual_range_contains`). I did not touch that file; I left it unchanged because it is unrelated to this task. The unit and integration tests all pass.
- `python3 tools/check_public_tree.py --range origin/main..HEAD` passes.
