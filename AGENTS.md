# oxidezap

Unofficial WhatsApp client on top of [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust).
GPUI front end. The same tree builds a desktop app and a web front end for
`wasm32-unknown-unknown`. **The desktop build is stable Rust; the web build
needs nightly**, because `-Z build-std` is nightly-only and the standard
library has to be rebuilt with the atomics feature on — see
[docs/building.md](docs/building.md) before provisioning a toolchain.

**This file holds decisions, not inventories.** Anything countable — which
crates exist, which dependency does a job, what a command's flags are, what the
module weighs — is derived from the tree and goes stale here faster than it goes
stale there. Where this file names a source of truth, read it rather than
trusting the sentence next to it.

## Shape

There is exactly **one WhatsApp session per user, and it lives in the daemon.**
A front end holds no session, no store and no media; it speaks a line protocol
to the daemon over whatever transport the platform has. On the web the page
starts a daemon in its own address space — same protocol, no process — so the
rule holds there too, and a second tab is a front end onto the first.

The layering, which is what a crate's placement has to respect:

- **core** is domain types, and they *are* the wire format. No UI, no I/O.
- **audio**, **video**, **chat-store** are capability crates. No UI, and each
  owns its own platform split rather than exporting one.
- **session** owns the WhatsApp connection and the devices. It names no
  platform except inside the modules that exist to be split.
- **ipc** is the protocol and the client end of the transport; **daemon** is the
  state every front end observes, plus the process around it.
- **gui** is a front end, and **never depends on session** — that is the rule,
  and its manifest is where a violation would show. On wasm it does depend on
  daemon, which is the same rule rather than an exception: a page has no process
  to reach one in. The manifest comments the gating.
- **plugin-abi / plugin-host / plugin-sdk** are the wasm ABI, the host that runs
  modules inside the daemon, and the SDK a plugin is written against.

`Cargo.toml`'s `members` is the list of crates, and each crate's `lib.rs` header
says what that one is for. Read those; do not trust a table for it. Note that a
directory name and a package name differ in at least one place.

Two directories sit outside the workspace on purpose, each carrying its own
`[workspace]` table: `examples/` (plugins link imports only the daemon provides,
so a host build fails at every `oxi_*` symbol) and `xtask/` (it takes no
dependencies at all, so the Pages job can compile it from a sparse checkout).
Only the first is in `exclude`; the second is simply not a member. The reasoning
is commented at both. `xtask/` has its own CI job; **`examples/` has none, and
nothing in CI builds either plugin** — the one test that loads a built module is
`#[ignore]`d, so a change there is checked by whoever makes it or not at all.

## Build & verify

`.github/workflows/ci.yml` is what actually gates a pull request — **check the
flags there rather than copying them from here**, since the two drift and the
workflow is the one that is right. As of writing it runs, per job:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features

# The tooling is its own workspace, so none of the above compiles a byte of it.
cargo fmt --manifest-path xtask/Cargo.toml --all -- --check
cargo clippy --manifest-path xtask/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path xtask/Cargo.toml
```

`cargo xtask help` lists the repository's own tooling — prefer asking it to
assuming, since tasks are added there. Running the client is two binaries and
the window looks for the daemon beside itself; `cargo build --release` then run
the front-end binary. The web build, plugin builds, the browser test runner and
the profiling and source-map builds are in
**[docs/building.md](docs/building.md)**.

## Rules that are not obvious

These are decisions. None is derivable by reading the code that obeys them,
which is why they are here and the inventories are not.

- **Never pin the `whatsapp-rust` crates individually by `rev`.** They resolve
  from one git source on one branch so `cargo update` moves them together; a
  mismatch surfaces as "expected `Jid`, found `Jid`" and reads like a compiler
  bug. `Cargo.toml` has the source and the current set.
- **Colours come from `cx.theme()`.** A literal is invisible to theme switching
  and drifts. There are a couple of deliberate exceptions, and
  `crates/gui/src/theme/` is where one has to argue for itself — the palette
  and its override table are there, so a colour with no token is visible.
- **Sizes come from the rem**, never from `px` literals — the window's size
  reaches the interface as one factor on the base font, applied once from the
  root's render pass. A component never learns that small screens exist. Follow
  the callers of the fit helper in `crates/gui/src/theme/metrics.rs`.
- **Render helpers take `&App` and return `impl IntoElement + use<>`** — without
  `use<>` the 2024 capture rules make them inherit a lifetime the virtual list's
  closure rejects.
- **The trimmed SQLite build is deliberate, and FTS5 must stay** — the search
  index is built on it. The feature list is in `.cargo/config.toml`.
- **The store is one file.** Device identity, Signal state and chat history
  share a database keyed by device id, so a partial wipe orphans everything
  behind the new device; the wipe deletes the file and its `-wal`/`-shm`.
- **A *transport's* platform split lives in exactly two places** — the endpoint
  side under `crates/ipc/src/endpoint/` and the listener side under
  `crates/daemon/src/listener/`. A new transport is added *there*; everything
  above them — framing, requests, the protocol — is written once, and the module
  headers in both say so. This is the rule for transports only: a capability
  crate owns its own split, which is why `audio/src/web/`, `video/src/web/` and
  `session/src/exec/` are where they are and not under ipc or daemon.
- **A page has no threads, and several std/tokio APIs compile for it and fail at
  run time** — `std::thread::spawn`, `tokio::time`, `spawn_blocking`. The
  session's `exec/` module is the seam that answers them; go through it rather
  than naming a clock or a pool. What decides where work goes is what the work
  *is*, not where the code lives.
- **A browser API never gets a view into wasm memory.** The module is built with
  `--shared-memory`, so the specs refuse a shared `ArrayBufferView`: copy before
  crossing out. There is a declared exception; the gotchas entry names it and
  the reason.
- **A plugin declares its capabilities once, and declaring grants nothing.**
  Approval is recorded separately, read live, and nothing loads from a directory
  another local account can write.
- **No real PII in tests**, including fixtures derived from captures.

## Where the reasoning lives

Read the relevant document before changing the code it describes — most entries
exist because the obvious alternative was tried and failed silently. They carry
the detail and the measurements this file deliberately does not.

- **[docs/architecture.md](docs/architecture.md)** — the crate map in prose, the
  theme, and how responsiveness derives from one number.
- **[docs/gotchas.md](docs/gotchas.md)** — non-obvious behaviour and why: the
  session/front-end split, calls, video, plugins, the store, the wire format.
- **[docs/web.md](docs/web.md)** — the page: its own daemon, the tab claim, the
  relay, media, service-worker caching, and what a page cannot do.
- **[docs/building.md](docs/building.md)** — every build beyond the ones above.
- **[docs/ci.md](docs/ci.md)** — the library dependency, and why the Actions
  cache budget decides how long a pull request waits.
- **[docs/plugin-abi.md](docs/plugin-abi.md)** — the contract for anyone not
  using the SDK. Load-bearing: a test loads the module it prints.
- **[docs/roadmap.md](docs/roadmap.md)** — known gaps and their reasoning.

Numbers in those documents are measurements, each true of the commit that took
it. Re-measure before relying on one; **a number is about the difference that
produced it**, which docs/ci.md explains at the cost of having got it wrong once.
