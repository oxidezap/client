# Building & verifying

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # what CI enforces
cargo test --workspace

# The tooling is its own workspace (see `xtask/` above), so none of the three
# lines above compiles a byte of it. CI has a job that does.
cargo fmt --manifest-path xtask/Cargo.toml --all
cargo clippy --manifest-path xtask/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path xtask/Cargo.toml
cargo xtask help    # what there is to run; from the repository root

# Running it: two binaries, and the window looks for the daemon beside itself.
cargo build --release --bin oxidezap --bin oxidezapd && ./target/release/oxidezap

# A plugin. Its own workspace, its own target, and the file's name is its id.
# `examples/template` is the same three commands; `cargo test` in either runs
# its handlers against the SDK's test host, with no daemon and no wasm.
# `RUSTFLAGS=` because the root's `.cargo/config.toml` sets `+atomics` and
# `--shared-memory` for this target — that target is the *web front end* — and
# cargo joins those into any build under this directory. A plugin built with
# them has a shared memory, which the host refuses outright.
cd examples/autoreply && RUSTFLAGS= cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/autoreply.wasm ~/.local/share/oxidezap/plugins/
# And the one test that exercises the real SDK against the real host. Back at
# the root first: the example is its own workspace and the root excludes it, so
# from in there cargo cannot resolve the host crate at all.
cd ../.. && cargo test -p oxidezap-plugin-host --all-features -- --ignored
```

The same window as a page:

```bash
# Needs nightly: `-Z build-std` is nightly-only, and the standard library has
# to be rebuilt with the atomics target feature on — gpui_web runs its
# background executor on real workers, and the prebuilt std is not compiled
# for that. The link flags are in /.cargo/config.toml.
rustup toolchain install nightly --component rust-src --target wasm32-unknown-unknown
cargo install trunk

# Serves on http://127.0.0.1:8080 with the two isolation headers set. On
# GitHub Pages a service worker adds them instead, because a static host has
# no way to.
# Through the task: trunk cannot forward arguments to cargo, so it is what
# sets the toolchain and `CARGO_UNSTABLE_BUILD_STD`. Run it from the root —
# the alias in /.cargo/config.toml names a manifest path.
TRUNK_ACTION=serve cargo xtask web build

# And the daemon it attaches to. `--web` alone is loopback on the port the
# page looks for; localhost is served without being named. It logs where the
# token file is, not the token — that is a bearer credential and a log is
# what people paste into issues. The token goes after a `#`, never a `?`: a
# query reaches whoever served the page, a fragment never leaves the browser.
cargo run --bin oxidezapd -- --web

# The same bundle with its symbols, for when a profile or a panic trace has
# to name something. `[profile.web-debug]` inherits `[profile.web]` and turns
# `strip` off, so it is the build that misbehaved rather than a different one
# — and `-g` in `data-wasm-opt-params` is what stops wasm-opt throwing the
# name section away again.
WEB_PROFILE=debug cargo xtask web build

# And the same again with its source *lines*, which is what a browser needs to
# say `crates/gui/src/app.rs:412` rather than a function name. Three things
# make that work and the task does all three: `[profile.web-dwarf]` keeps the
# DWARF, the build skips wasm-opt — which moves the code the line table
# describes, and rewrites the table only for the transformations it knows how
# to follow — and `cargo xtask web map` projects `.debug_line` into a source
# map beside the module, pointing the module at it with a `sourceMappingURL`
# section. DWARF is what an extension reads; the map is what DevTools reads on
# its own, in every engine. It is not the build to *profile*, though — a flame
# chart reads the name section, which `debug` above already keeps, and this one
# skips wasm-opt and so is a different code layout. See the gotcha.
WEB_PROFILE=dwarf cargo xtask web build
cargo xtask web map            # the map again, over a module already built
```

`TRUNK_ACTION=serve` is refused for that profile rather than served unmapped:
the map is written after the build, a serve never finishes, and a serve that
rebuilt would leave the map describing a module that is gone — silently, since
a stale map still parses. Build it and serve `web/dist` with any static
server.

The web half of the daemon — the OPFS folder a page installs plugins into —
is `cfg`-gated to wasm, so `cargo test --workspace` compiles none of it. It
has tests that run in a real browser instead, and CI runs them:

```bash
# The driver must match the browser's major version. `RUSTFLAGS` here
# *replaces* the root's wasm flags, which is deliberate: those are the web
# front end's, and a shared memory would need headers this runner does not
# serve. The Web Locks cfg is the one that has to stay.
CHROMEDRIVER=$(which chromedriver) \
RUSTFLAGS='--cfg web_sys_unstable_apis' \
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
  cargo test -p oxidezap-daemon --lib --target wasm32-unknown-unknown
```

Type-checking the web build without the whole bundle:

```bash
cargo +nightly check -p oxidezap-gui --target wasm32-unknown-unknown -Z build-std=std,panic_abort
```

The session builds for that target too, and is checked separately because
nothing in the page depends on it yet — a break there would otherwise go
unnoticed until something does:

```bash
cargo +nightly check -p oxidezap-session --target wasm32-unknown-unknown -Z build-std=std,panic_abort
```

Stable Rust. Debug builds keep gpui at opt-level 3, because without it the UI is
unusable.

