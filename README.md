# oxidezap

An unofficial WhatsApp client in Rust, built on
[whatsapp-rust](https://github.com/oxidezap/whatsapp-rust).

Not affiliated with, endorsed by, or connected to WhatsApp or Meta.

## Status

Early. Pairing, chats with durable history, media, plugins and 1:1 voice and
video calls work, on the desktop and in a browser. Read
[known limitations](#known-limitations) before relying on it.

## Layout

The WhatsApp connection lives in one background process, `oxidezapd`: it holds
the session, owns the store, shows a tray icon on Linux, Windows and macOS and serves front ends
over a per-user local transport — a Unix socket, or a named pipe on Windows.
`oxidezap` is the GPUI desktop window; it owns no session and starts the daemon
when none is running. One session per user, one process that opens the
database, however many windows you like.

| Crate | What it owns |
| --- | --- |
| `oxidezap-core` | Domain types: chats, messages, calls, UI events. Also the daemon's wire format. |
| `oxidezap-audio` | Capture, playback, Opus encoding, waveforms. |
| `oxidezap-video` | Camera capture and H.264 encoding for calls. |
| `oxidezap-chat-store` | SQLite chat history materialized from the event stream, with FTS5 search. |
| `oxidezap-session` | Connection, event stream, sends, store hydration. |
| `oxidezap-ipc` | The protocol between the daemon and its front ends, and the client end of the transport. |
| `oxidezap-daemon` | `oxidezapd`: the session, the socket and the tray. |
| `oxidezap-gui` | `oxidezap`: GPUI front end, plus video decode. Also builds to WebAssembly. |
| `oxidezap-plugin-abi` | The wasm ABI: constants and the widget-tree codec. `no_std`, no dependencies. |
| `oxidezap-plugin-host` | Runs `.wasm` plugins inside the daemon: discovery, the sandbox, approvals. |
| `oxidezap-plugin` | The Rust SDK a plugin is written against. |

## Install

Prebuilt binaries for Linux, macOS and Windows are attached to each
[release](https://github.com/oxidezap/client/releases). Builds of `main` are
published continuously under the `nightly` tag.

Each platform archive holds two binaries that belong together: `oxidezap` is
the window and `oxidezapd` holds the session. Keep them in the same directory
— the window looks for the daemon beside itself.

```bash
tar -xzf oxidezap-nightly-linux-x86_64.tar.gz
cd oxidezap-nightly-linux-x86_64
./oxidezap
```

The web front end ships beside them as `oxidezap-<version>-web.zip`: static
files to serve from any web server, with hosting notes in the archive.

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
gpui itself optimized (`[profile.dev.package.gpui]`), which is what makes them
usable at all.

## The web front end

The same window builds to WebAssembly. By default a page runs the whole client
— the session, the store, plugins and calls — under your account and nobody
else's, and the published bundle is still static.

Open it in as many tabs as you like. One tab holds the account and the others
are windows onto it, over the same protocol the desktop window speaks to
`oxidezapd`. Opening a second tab disconnects nothing, which is the one thing
WhatsApp Web will not do. Two costs: each tab is served its own copy of every
frame, and closing the tab that holds the account hands it to another, which
*restarts* the session rather than inheriting it — a second or two of
reconnecting, with nothing lost.

Building and serving it locally:

```bash
# Nightly, because the standard library has to be rebuilt with the atomics
# target feature on: the window runs its background work on real workers.
rustup toolchain install nightly --component rust-src --target wasm32-unknown-unknown
cargo install trunk

# Through the task: trunk cannot forward arguments to cargo, so this is what
# sets the toolchain and `CARGO_UNSTABLE_BUILD_STD`.
TRUNK_ACTION=serve cargo xtask web build
```

### Attaching to a daemon instead

A page can attach to an `oxidezapd` on your own machine over a WebSocket,
speaking the protocol the desktop window already speaks. Worth preferring where
you have one: the daemon survives the tab, gives each plugin a thread of its
own, can send media, and keeps your device keys in a `0700` directory rather
than in a browser's storage.

```bash
cargo run --bin oxidezapd -- --web
```

It logs the line to open and where the token is — not the token itself, which
is a bearer credential and a log is the one thing people paste into issues:

```text
web bridge listening on http://127.0.0.1:9527/ws (origins: loopback only)
point a page at #daemon=ws://127.0.0.1:9527/ws?token=<token>, where <token>
is the contents of /run/user/1000/oxidezap/web.token
```

That file is yours alone (`0600`, in your own runtime directory —
`$XDG_RUNTIME_DIR/oxidezap`, or `${TMPDIR:-/tmp}/oxidezap-$UID` where that is
unset). Paste it into
`http://127.0.0.1:8080/#daemon=ws://127.0.0.1:9527/ws?token=<token>`. Without
the token the page is refused: it is the whole of the admission check. A bare
<http://127.0.0.1:8080> names no daemon and runs its own session.

**After the `#`, never after a `?`.** A query string reaches whoever served the
page — for the hosted build, GitHub's servers and their logs — while browsers
strip the fragment before the request goes out. The page still reads a
`?daemon=` and says so in the console, because refusing it would not un-send
it; if you ever used one, the repair is a new token (`rm` the token file and
restart).

**The endpoint is off by default and should stay off unless you want it.** A
WebSocket is not subject to the same-origin policy, so any page in your browser
can try to reach `ws://127.0.0.1` — and this one carries your message history
and can send. Hence the token, which is what a loopback port lacks and a Unix
socket has in its peer uid. It also refuses to bind anywhere but loopback, and
refuses every browser origin except the loopback ones — `localhost`,
`127.0.0.1`, `[::1]`, on any port, which is `trunk serve` on your own machine —
and the ones you name:

```bash
# Serve a page published somewhere else — a Pages deployment, say.
oxidezapd --web --web-allow https://oxidezap.github.io
```

The page checks the other direction too: a `#daemon=` URL is honoured only for
loopback or for the page's own origin — the whole origin, scheme and port
included. So reaching the bridge from another machine is a tunnel's job (`ssh
-L 9527:127.0.0.1:9527`, which lands it back on loopback), and a reverse proxy
only works if it serves the page and the bridge at the same origin: a page on
GitHub Pages pointed at `wss://home.example/ws` is refused before it connects.

**Known gap: the daemon is not authenticated to the page.** The token proves
the page to the daemon and nothing proves the daemon to the page. Another
account on your machine could bind the port first, collect the token from a
bookmark opened while the daemon was down, and reuse it — the `Origin` header
is a string they control too. Closing it needs mutual authentication in the
protocol; until then, leave `--web` off on a machine you share with someone you
do not trust. See `endpoint_url` in `crates/ipc/src/endpoint/web.rs`.

### What a page cannot do

* **A page records and sends voice notes wherever the browser has an Opus
  encoder.** That question is asked of the browser rather than of the build —
  `AudioEncoder` is something an older one may not have — and it is asked
  before the microphone is offered, so a control that would always fail is not
  drawn. It is the only thing asked: a page holding its own session uploads
  through the library's buffered path, which needs only the one HTTP method a
  browser can answer, and a page attached to an `oxidezapd` stages the payload
  over the bridge for the daemon to upload.
* **WebGL by default, WebGPU on request.** `?backend=webgpu` asks for the
  faster one, `?backend=auto` for whatever the browser prefers. The default is
  conservative because WebGPU can pass its own probe and then fail building a
  pipeline, which reaches wgpu as a panic and leaves a window that never draws
  — observed on an ordinary Intel/Mesa laptop.
* **A call in a page needs `RTCPeerConnection`.** That is what carries the
  media where the page holds its own session, so it is the one thing asked
  before the call control is drawn at all; the microphone and speaker are
  WebAudio and the camera is `getUserMedia`. A browser with no `VideoEncoder`
  is not refused — a camera that will not open downgrades a call to voice
  rather than failing it — and a browser with no `VideoDecoder` cannot draw the
  peer's picture, which is asked separately and is also why videos in a
  conversation are not offered there. Attached to an `oxidezapd`, the call
  rings in the daemon, so it keeps running with every window closed.

## Data

State lives in one SQLite file under the platform data directory
(`~/.local/share/oxidezap/whatsapp.db` on Linux): device identity, Signal state
and chat history together. Deleting it unlinks the device and discards local
history, which is what the in-app "pair again" action does.

## Plugins

A plugin is a `.wasm` file dropped in `~/.local/share/oxidezap/plugins` — or,
in a page running its own session, added under Settings → Plugins, which keeps
it in the browser's own storage. Either way it runs inside the daemon — which
for a page holding its own session is the page itself, so its plugins share
that one agent instead of getting a thread each — and either way it sees the
account's events and can declare a small interface, a button in a chat header
or a section on the Settings screen, that the window draws in its own theme.
Adding or removing one changes what the *next* load runs: a tab reload, or a
daemon restart.

There is no WASI: the `oxidezap` import module is a plugin's entire outside
world, so a downloaded file cannot read the disk or open a socket because no
function exists that would. What it may do *to the account* — send, mark read,
show a typing indicator — is withheld until you say yes, and withdrawing that
answer takes effect on the plugin's next command.

Copy `examples/template` to start one, or read `docs/plugin-abi.md` if you are
writing in something other than Rust — the SDK is a convenience over that
document and has no privileged access.

## Known limitations

* Group calls and choosing an audio output device are drawn but disabled.
* Spacing does not yet follow the rem scale, so the UI ignores base-font zoom.
* A page's plugins share its single agent rather than getting a thread each;
  attaching it to an `oxidezapd` gives each one a thread. See
  [the web front end](#the-web-front-end).
* Uploading from a page holding its own session has not been measured against
  a real account: the CDN's CORS preflight is the part that is unverified. See
  `docs/roadmap.md`.

## License

MIT.
