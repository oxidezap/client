# oxidezap

An unofficial WhatsApp client in Rust, built on
[whatsapp-rust](https://github.com/oxidezap/whatsapp-rust).

Not affiliated with, endorsed by, or connected to WhatsApp or Meta.

## Status

Early. Pairing, chats with durable history, media and 1:1 voice calls work.
Read [known limitations](#known-limitations) before relying on it.

## Layout

The WhatsApp connection lives in `crates/session` and knows nothing about how
it is drawn, so a front end is a thin consumer of it. `crates/gui` is the
first one, a GPUI desktop app producing the `oxidezap` binary. A background
daemon (`oxidezapd`) and a TUI are the reason for the split; neither is
written yet.

| Crate | What it owns |
| --- | --- |
| `oxidezap-core` | Domain types: chats, messages, calls, UI events. No UI, no I/O. |
| `oxidezap-audio` | Capture, playback, Opus encoding, waveforms. |
| `oxidezap-chat-store` | SQLite chat history materialized from the event stream, with FTS5 search. |
| `oxidezap-session` | Connection, event stream, sends, store hydration. |
| `oxidezap-gui` | GPUI front end, plus video decode. |

## Install

Prebuilt binaries for Linux, macOS and Windows are attached to each
[release](https://github.com/oxidezap/client/releases). Builds of `main` are
published continuously under the `nightly` tag.

Each asset is the binary itself, named for its platform. On Linux and macOS
it arrives without the execute bit:

```bash
chmod +x oxidezap-linux-x86_64
./oxidezap-linux-x86_64
```

The binaries are unsigned, so macOS Gatekeeper and Windows SmartScreen will
object. On macOS, clear the quarantine flag before the first run:

```bash
xattr -dr com.apple.quarantine oxidezap-macos-aarch64
```

## Build

Stable Rust. On Linux you also need the ALSA, X11/Wayland and fontconfig
development packages:

```bash
sudo apt install libasound2-dev libxkbcommon-dev libxkbcommon-x11-dev \
  libwayland-dev libxcb1-dev libfontconfig1-dev libfreetype6-dev
cargo run --release
```

The first build compiles the GPUI tree and takes a while. Debug builds keep
gpui itself optimized (see `[profile.dev.package.gpui]`), which is what makes
them usable at all.

## Data

State lives in one SQLite file under the platform data directory
(`~/.local/share/oxidezap/whatsapp.db` on Linux): device identity, Signal
state and chat history together. Deleting it unlinks the device and discards
local history, which is exactly what the in-app "pair again" action does.

## Known limitations

* Voice calls only. The library's call facade is 1:1 audio, so the video call
  button places a voice call.
* Media bubbles re-download on demand after a restart.
* Reactions persist but are not hydrated into the UI at startup.
* Spacing does not yet follow the rem scale, so the UI ignores base-font zoom.

## License

MIT.
