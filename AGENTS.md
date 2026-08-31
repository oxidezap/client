# oxidezap

Unofficial WhatsApp client on top of [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust).
Rust, stable toolchain, GPUI front end. The same tree builds a desktop app and a
web front end for `wasm32-unknown-unknown`.

## Shape

There is exactly **one WhatsApp session per user, and it lives in the daemon.**
A front end holds no session, no store and no media; it speaks a line protocol to
the daemon over a socket, a named pipe, a WebSocket or a `BroadcastChannel`. On
the web the page starts a daemon in its own address space — same protocol, no
process — so the rule holds there too.

| Crate | What it is |
|---|---|
| `oxidezap-core` | Domain types: chats, messages, calls, UI events. No UI, no I/O. |
| `oxidezap-audio` | Capture, playback, Opus, waveforms. cpal on a desktop, WebAudio + `AudioEncoder` in a page. |
| `oxidezap-video` | Camera capture and H.264 encode. nokhwa + OpenH264, or `getUserMedia` + `VideoEncoder`. No decode. |
| `oxidezap-chat-store` | Materializes the library's events into chats, messages, receipts and an FTS5 search index. Owns its schema. |
| `oxidezap-session` | The WhatsApp connection: events, sends, hydration, calls, devices. Names no platform above `net/`, `exec/`, `video/`. |
| `oxidezap-ipc` | The wire protocol and the blocking client end (`Endpoint`). Core's types *are* the wire format. |
| `oxidezap-logging` | The log level, as a setting rather than a launch argument, shared by both processes. |
| `oxidezap-daemon` | The library (state hub, session bridge, protocol — builds for wasm) plus the binary `oxidezapd` (socket, tray, signals). |
| `oxidezap-plugin-abi` | The wasm ABI: constants and the widget-tree codec. `no_std`, no dependencies. |
| `oxidezap-plugin-host` | Runs `.wasm` plugins inside the daemon: discovery, sandbox, host half of the ABI. wasmi, no JIT. |
| `oxidezap-plugin` | The Rust SDK a plugin is written against. Not a dependency of anything here. |
| `oxidezap-gui` | GPUI front end, binary `oxidezap`. Owns video decode. Also builds for wasm. |

Outside the workspace on purpose: `examples/` (plugins — link imports only the
daemon provides) and `xtask/` (the web build, bundle checks and the `gh-pages`
publisher — it takes no dependencies so the Pages job can compile it from a
sparse checkout). Both have their own CI jobs.

## Build & verify

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # what CI enforces
cargo test --workspace

# The tooling is its own workspace, so none of the above compiles a byte of it.
cargo clippy --manifest-path xtask/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path xtask/Cargo.toml
cargo xtask help

# Running it: two binaries, and the window looks for the daemon beside itself.
cargo build --release --bin oxidezap --bin oxidezapd && ./target/release/oxidezap
```

The web build, plugin builds, the browser test runner and the profiling and
source-map builds are in **[docs/building.md](docs/building.md)**.

## Rules that are not obvious

- **Never pin the eight `whatsapp-rust` crates individually by `rev`.** They
  resolve from one git source on one branch so `cargo update` moves them
  together; a mismatch surfaces as "expected `Jid`, found `Jid`".
- **Colours come from `cx.theme()`.** A literal is invisible to theme
  switching. The only exceptions are message bubbles and the QR raster.
- **Render helpers take `&App` and return `impl IntoElement + use<>`** — without
  `use<>` the 2024 capture rules make them inherit a lifetime the virtual list
  rejects.
- **Sizes come from the rem**, never from `px` literals. One number, in one
  place: `theme::metrics::viewport_fit`. A component never learns that small
  screens exist.
- **FTS5 must stay** in the trimmed SQLite build in `.cargo/config.toml`.
- **The store is one file.** A partial wipe orphans history behind the new
  device id; `wipe_local_state` deletes the database and its `-wal`/`-shm`.
- **A platform split lives in exactly two places** — `ipc/endpoint/` and
  `daemon/listener/`. Everything above them is written once.
- **A page has no threads, no `spawn_blocking` and no `tokio::time`.** All three
  compile for wasm and fail at run time. Use `exec::sleep`/`exec::with_timeout`,
  and split work by what it *is* rather than by where the code lives.
- **A browser API never gets a view into wasm memory** — this module is built
  with `--shared-memory`, so copy before crossing out. WebCodecs is the
  documented exception.
- **A plugin declares its capabilities once and is approved separately.**
  Declaring grants nothing; nothing loads from a directory another account can
  write.
- **No real PII in tests**, including fixtures derived from captures.

## Where the reasoning lives

Read the relevant document before changing the code it describes — most entries
exist because the obvious alternative was tried and failed silently.

- **[docs/architecture.md](docs/architecture.md)** — the crate map in full, the
  theme, and how responsiveness derives from one number.
- **[docs/gotchas.md](docs/gotchas.md)** — non-obvious behaviour and why: the
  session/front-end split, calls, video, plugins, the store, the wire format.
- **[docs/web.md](docs/web.md)** — the page: its own daemon, the tab claim, the
  relay, media, service-worker caching, and what a page cannot do.
- **[docs/building.md](docs/building.md)** — every build beyond the three above.
- **[docs/ci.md](docs/ci.md)** — the library dependency, and why the 10 GB
  Actions cache decides how long a pull request waits.
- **[docs/plugin-abi.md](docs/plugin-abi.md)** — the contract for anyone not
  using the SDK. Load-bearing: a test loads the module it prints.
- **[docs/roadmap.md](docs/roadmap.md)** — known gaps and their reasoning.
