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

The same window builds to WebAssembly and runs in a browser. By default it
runs the whole client there — the session, the store and the window — under
your account and nobody else's; the published bundle is still static, because
nothing about that needs a server.

It can attach to an `oxidezapd` on your own machine instead, over a WebSocket
and speaking the protocol the desktop window already speaks. That is worth
preferring where you have one: a desktop daemon holds calls, survives the tab
closing, and keeps your device keys in a `0700` directory rather than in a
browser's storage. Naming one with `#daemon=` is how you choose it, and the
rest of this section is how.

```bash
# Nightly, because the standard library has to be rebuilt with the atomics
# target feature on: the window runs its background work on real workers.
rustup toolchain install nightly --component rust-src --target wasm32-unknown-unknown
cargo install trunk

# Through the script, which is the only thing that sets the toolchain and
# `CARGO_UNSTABLE_BUILD_STD`. Trunk cannot forward arguments to cargo, so
# `trunk serve -- -Z build-std=…` passes them to the dev server instead and
# the build fails on whatever toolchain happens to be default.
TRUNK_ACTION=serve ./web/build.sh
```

The page will start its own session. To point it at a daemon instead, run one
in another terminal with its web endpoint turned on:

```bash
cargo run --bin oxidezapd -- --web
```

It logs the line to open and where the token is — not the token itself, which
is a bearer credential and a log is the one thing people paste into issues:

```text
web bridge listening on http://127.0.0.1:9527/ws (origins: loopback only)
point a page at #daemon=ws://127.0.0.1:9527/ws?token=<token>, where <token>
is the contents of $XDG_RUNTIME_DIR/oxidezap/web.token
```

That file is yours alone (`0600`, in your own runtime directory), so:

```bash
cat "$XDG_RUNTIME_DIR/oxidezap/web.token"
```

Open `http://127.0.0.1:8080/#daemon=ws://127.0.0.1:9527/ws?token=<token>`
with it pasted in. Without the token the page reaches the endpoint and is
refused: it is the whole of the admission check. A bare
<http://127.0.0.1:8080> names no daemon at all, which is not a refusal — it
is the default, and that page runs its own session.

**After the `#`, not after a `?`, and that is not cosmetic.** A page's query
string is part of the request line and reaches whoever served the page — for
the hosted build, that is GitHub's servers and their logs. The fragment is
never sent: browsers strip it before the request goes out. Since the token
is a bearer credential, putting it in the query would hand a copy to the
static host in exchange for nothing. The page still reads a `?daemon=` and
says so in the console, because refusing it would not un-send it — but if
you ever used one, the repair is a new token, which is `rm` on the token
file and a restart.

**Known gap: the daemon is not authenticated to the page.** The token proves
the page to the daemon and nothing proves the daemon to the page. Another
account on your machine could bind the port first, collect the token from a
bookmark opened while the daemon was down, and reuse it afterwards — the
`Origin` header is a string they control too. The native endpoint has no such
gap, because a Unix socket has a peer uid to check and a TCP port does not.
Closing it needs mutual authentication in the protocol; until then this is a
reason to leave `--web` off on a machine you share with someone you do not
trust. See the note on `endpoint_url` in `crates/ipc/src/endpoint/web.rs`.

**That endpoint is off by default, and should stay off unless you want it.**
A WebSocket is not subject to the same-origin policy, so any page open in
your browser can try to reach `ws://127.0.0.1` — and this one carries your
message history and can send.

So it requires a token, which the daemon prints on startup and keeps in your
own state directory. A loopback port is reachable by every account on the
machine, unlike the Unix socket, which lives in a directory only you can read
— the token is what carries that guarantee across. It also refuses to bind
anywhere but loopback, and refuses every browser origin except the ones you
name and `localhost`, which is built in:

```bash
# Serve a page published somewhere else — a Pages deployment, say.
oxidezapd --web --web-allow https://oxidezap.github.io
```

`localhost` and `127.0.0.1` are served without being named, because that is
`trunk serve` on your own machine. Point a page at a daemon with
`#daemon=ws://host:port/ws?token=…` — the whole line the daemon logs when it
starts. The URL is honoured only for this machine or the origin the page
itself came from, and the token has to match either way.

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
* **Calls need a daemon, and then have no picture.** Where the page is
  attached to an `oxidezapd` — `#daemon=ws://…` — calls work: they ring in the
  daemon, which is where the microphone and the codec already were, so the
  page places and answers them like any other front end, and a call still runs
  with every window closed. What it cannot do even then is *decode* the
  picture, for the same reason it cannot decode a clip, so a video call's
  panes say the picture needs the desktop app rather than waiting on one that
  is not coming.

  A page running its **own** session has no daemon to ring in and refuses
  every call action, incoming and outgoing alike: the microphone and the codec
  are the same C libraries a browser has none of. `MediaRecorder` and
  WebCodecs are the ways in, and both are API changes rather than backends.

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
