# oxidezap

An unofficial WhatsApp client in Rust, built on
[whatsapp-rust](https://github.com/oxidezap/whatsapp-rust).

Not affiliated with, endorsed by, or connected to WhatsApp or Meta.

## Status

Early. Pairing, chats with durable history, media and 1:1 voice calls work.
Read [known limitations](#known-limitations) before relying on it.

## Layout

The WhatsApp connection lives in one background process, `oxidezapd`. It holds
the session, owns the store, shows a tray icon on Linux and serves front ends
over a per-user local transport — a Unix socket, or a named pipe on Windows. `oxidezap` is the GPUI desktop window; it owns no session of
its own and starts the daemon when none is running. One session per user, one
process that opens the database, however many windows you like.

| Crate | What it owns |
| --- | --- |
| `oxidezap-core` | Domain types: chats, messages, calls, UI events. No UI, no I/O — and the daemon's wire format. |
| `oxidezap-audio` | Capture, playback, Opus encoding, waveforms. |
| `oxidezap-chat-store` | SQLite chat history materialized from the event stream, with FTS5 search. |
| `oxidezap-session` | Connection, event stream, sends, store hydration. |
| `oxidezap-ipc` | The protocol between the daemon and its front ends, and the client end of the transport. No runtime. |
| `oxidezap-daemon` | `oxidezapd`: the session, the socket and the tray. |
| `oxidezap-gui` | `oxidezap`: GPUI front end, plus video decode. Also builds to WebAssembly — see [the web front end](#the-web-front-end). |

## Install

Prebuilt binaries for Linux, macOS and Windows are attached to each
[release](https://github.com/oxidezap/client/releases). Builds of `main` are
published continuously under the `nightly` tag.

Each asset is an archive holding two binaries that belong together:
`oxidezap` is the window and `oxidezapd` holds the session. Keep them in the
same directory — the window looks for the daemon beside itself.

```bash
tar -xzf oxidezap-nightly-linux-x86_64.tar.gz
cd oxidezap-nightly-linux-x86_64
./oxidezap
```

The binaries are unsigned, so macOS Gatekeeper and Windows SmartScreen will
object. On macOS, clear the quarantine flag before the first run:

```bash
xattr -dr com.apple.quarantine oxidezap oxidezapd
```

## Build

Stable Rust. On Linux you also need the ALSA, X11/Wayland and fontconfig
development packages:

```bash
sudo apt install libasound2-dev libxkbcommon-dev libxkbcommon-x11-dev \
  libwayland-dev libxcb1-dev libfontconfig1-dev libfreetype6-dev

# Both, because the window starts the daemon beside itself.
cargo build --release --bin oxidezap --bin oxidezapd
./target/release/oxidezap
```

The first build compiles the GPUI tree and takes a while. Debug builds keep
gpui itself optimized (see `[profile.dev.package.gpui]`), which is what makes
them usable at all.

## The web front end

The same window builds to WebAssembly and runs in a browser. It is the front
end only: it holds no session, opens no connection to WhatsApp and keeps no
store, so it attaches to an `oxidezapd` on your own machine over a WebSocket,
speaking the protocol the desktop window already speaks.

That makes the published bundle static — nothing serves it but a file host —
while the daemon it talks to stays yours.

```bash
# Nightly, because the standard library has to be rebuilt with the atomics
# target feature on: the window runs its background work on real workers.
rustup toolchain install nightly --component rust-src --target wasm32-unknown-unknown
cargo install trunk

cd web && trunk serve -- -Z build-std=std,panic_abort
```

and, in another terminal, the daemon with its web endpoint turned on:

```bash
cargo run --bin oxidezapd -- --web
```

Then open <http://127.0.0.1:8080>.

**That endpoint is off by default, and should stay off unless you want it.**
A WebSocket is not subject to the same-origin policy, so any page open in
your browser can try to reach `ws://127.0.0.1` — and this one carries your
message history and can send. It therefore refuses to bind anywhere but
loopback, and it refuses every browser origin except the ones you name and
`localhost`, which is built in:

```bash
# Serve a page published somewhere else — a Pages deployment, say.
oxidezapd --web --web-allow https://oxidezap.github.io
```

`localhost` and `127.0.0.1` are served without being named, because that is
`trunk serve` on your own machine. Pass `?daemon=ws://host:port/ws` to point
a page at a daemon other than the default one; it is honoured only for this
machine or the origin the page itself came from.

Reaching the bridge from another machine is a tunnel's job — `ssh -L
9527:127.0.0.1:9527`, or a reverse proxy that terminates TLS and
authenticates. It will not bind a public address itself: off the loopback its
only check would be an `Origin` header the client chooses, and the session
would cross the network in the clear.

What the web build cannot do, and reports rather than pretends:

* **No video.** The H.264 decoder is a C library and does not build for
  `wasm32-unknown-unknown`. Clips keep their thumbnail and say so.
* **No recording voice notes.** A voice note is Opus, and libopus is C too.
  Playback works, because the browser decodes Opus itself.
* **No calls.** They ring in the daemon, which is where the microphone was
  already — a call still works with the window closed, on the desktop.

## Data

State lives in one SQLite file under the platform data directory
(`~/.local/share/oxidezap/whatsapp.db` on Linux): device identity, Signal
state and chat history together. Deleting it unlinks the device and discards
local history, which is exactly what the in-app "pair again" action does.

## Known limitations

* Voice calls only. The library's call facade is 1:1 audio, so video calls,
  group calls and output-device selection are drawn but disabled.
* Spacing does not yet follow the rem scale, so the UI ignores base-font zoom.
* The web build is the window only. It reaches a daemon on your own machine
  directly, and one elsewhere only through a tunnel you set up; see [the web
  front end](#the-web-front-end) for what it drops.

## License

MIT.
