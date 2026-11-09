# Agent guide

rewrite-bridge gives game-rewrite and passthrough-mod developers shared asset types, a provider interface and a protocol for linking running games.
The core, optional `link` feature and separate rewrite-bridge-bevy package form the shared layer used by the readers and language ports.

## Toolchain

- CI uses stable Rust; the README reports tests with 1.99.1 and no tested older minimum for the core.
- The core's documented API floor is Rust 1.88 (`slice::as_chunks`); Bevy 0.20 declares 1.97.1.
- Native macOS builds require 11.0+; set `MACOSX_DEPLOYMENT_TARGET=11.0`.
- The default core has no dependencies; enable `link` for TCP/UDP and pixel files.
- Bevy features: `viewer` for offscreen examples, `window` for windowing, `compose` for depth compositing.

## CI commands

Core checks on Linux, macOS and Windows (`ci.yml`):

```sh
cargo test
cargo test --features link
python3 tests/vectors/link/gen.py --check
cargo run --example cube_provider
cargo run --example conformance -- --id right
cargo clippy --all-targets --features link -- -D warnings
```

The vector-generator check is Linux-only. Tests compile the core; the README also uses `cargo build`.
Bevy checks on Linux/macOS:

```sh
cargo test -p rewrite-bridge-bevy
cargo build -p rewrite-bridge-bevy --example viewer --features viewer
cargo clippy -p rewrite-bridge-bevy --all-targets --features viewer -- -D warnings
cargo test -p rewrite-bridge-bevy --features compose,viewer --lib gpu_report -- --nocapture
cargo test -p rewrite-bridge-bevy --features compose,viewer
cargo build -p rewrite-bridge-bevy --example guest_compose --features compose,viewer
cargo clippy -p rewrite-bridge-bevy --all-targets --features compose,viewer -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p rewrite-bridge-bevy --features compose --no-deps
```

Windows additionally compiles/lints with `cargo check -p rewrite-bridge-bevy --all-targets --features compose,viewer`
and `cargo clippy -p rewrite-bridge-bevy --all-targets --features compose,viewer -- -D warnings`.
GPU tests report missing adapters and may skip; `RB_REQUIRE_GPU=1` makes an absent adapter fail.
WASI CI installs the `wasm32-wasip2` target and Wasmtime 50.0.1, then runs:

```sh
cargo clippy --target wasm32-wasip2 --features link -- -D warnings
CARGO_TARGET_WASM32_WASIP2_RUNNER="timeout 300 wasmtime run -S inherit-network --dir /tmp::/tmp" cargo test --target wasm32-wasip2 --features link --test link_wasi
```

Gate/workflow regression tests:

```sh
python3 -m venv .workflow-tests
.workflow-tests/bin/python -m pip install -r tools/requirements-test.txt
.workflow-tests/bin/python -m unittest discover -s tools -p 'test_*.py'
.workflow-tests/bin/python -m unittest discover -s tests/interop -p test_results.py
```

`link-interop.yml` checks peer vector copies using `scripts/sync-link-vectors.sh --check "peers/$r"`
for `link-java`, `link-c` and `link-cs` at `tests/interop/pins.txt` revisions, then runs `tests/interop/run.sh --build`.
Its Linux environment uses JDK 21, .NET SDK 8 and a C compiler, with `RBL_LINK_JAVA_DIR`,
`RBL_LINK_C_DIR` and `RBL_LINK_CS_DIR` pointing to the checked-out peers and `RBL_LOG_DIR` to the logs directory.
Release CI builds reference peers with `cargo build --release --locked --target <target> --features link --example fake_host --example fake_guest --example interop_peer`.
Its targets are `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc`.

## Code map

- `src/lib.rs`, `transform.rs`: model, mesh, material, image, skin and transform types.
- `src/animated.rs`, `animation.rs`, `scene.rs`: animated assets, clips and scenes.
- `src/conformance/`: provider semantic checks; `src/link/`, `link.rs`: protocol and pixel I/O.
- `rewrite-bridge-bevy/`: renderer plugin, viewer and compositor.
- `docs/CONTRACT.md`, `INTEGRATING.md`, `WRITING_A_PROVIDER.md`: protocol and integration guides.
- `tests/`, `examples/`, `scripts/`: generated vectors, interop, reference peers and provider examples.

## Project rules

- Use metres, right-handed +Y up, +Z front and counter-clockwise front faces.
- Colour/emissive textures are sRGB; other textures are linear; roughness is G and metalness B.
- Tangent-space normal maps use OpenGL's green-up convention; validate provider models and conformance.
- Keep one rewrite-bridge minor-version line across consumers to avoid duplicate incompatible types.
- Link implementations follow the contract and share generated vectors; update peer pins deliberately with dependent changes.
- Providers read the player's installation; no game data belongs in the bridge.

## Public files

- Never commit game files, extracted assets, renders, decompiled code or secrets.
- The public-tree gate needs Python 3.9+ and Git; run it from the repository root:
  `python3 tools/check_public_tree.py` (checks Git's index, including staged new files).
- CI checks committed changes with `python3 tools/check_public_tree.py --range BASE..HEAD`.

## Contributing, license and AI

- Open an issue to discuss a problem or send a pull request with the change.
- Before a pull request, run the applicable CI commands above and the public-tree gate.
- License: MIT OR Apache-2.0, at your option; see `LICENSE-MIT` and `LICENSE-APACHE`.

The code was written with AI coding agents (Anthropic's Claude and OpenAI's GPT models) under the maintainer's direction.
AI-assisted contributions are welcome when their use is disclosed and the changes are reviewed.
