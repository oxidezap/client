# oxidezap

An unofficial WhatsApp client in Rust, built on [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust).

Not affiliated with, endorsed by, or connected to WhatsApp or Meta.

## Status

Early. Pairing, chats with durable history, media, and 1:1 voice calls work; see
[known limitations](#known-limitations) before relying on it.

## Layout

The connection lives in `crates/session` and knows nothing about how it is drawn,
so a front end is a thin consumer of it. `crates/gui` is the first one — a GPUI
desktop app producing the `oxidezap` binary. A background daemon (`oxidezapd`)
and a TUI are the reason the split exists; neither is written yet.

`crates/core` holds the domain types both sides speak, and `crates/audio` the
capture, playback and Opus encoding shared by voice messages and calls.

## Build

Stable Rust. On Linux you also need the ALSA, X11/Wayland and fontconfig
development packages:

```bash
sudo apt install libasound2-dev libxkbcommon-dev libxkbcommon-x11-dev \
  libwayland-dev libxcb1-dev libfontconfig1-dev libfreetype6-dev
cargo run --release
```

The first build compiles the GPUI tree and takes a while. Debug builds keep gpui
itself optimized (see `[profile.dev.package.gpui]`), which is what makes them
usable at all.

## Data

State lives in one SQLite file under the platform data directory
(`~/.local/share/whatsapp-rust-desktop/whatsapp.db` on Linux): device identity,
Signal state and chat history together. Deleting it unlinks the device and
discards local history.

## Known limitations

- Voice calls only — the library's call facade is 1:1 audio, so the video call
  button places a voice call.
- Media bubbles re-download on demand after a restart.
- Reactions persist but are not hydrated into the UI at startup.
- Release binaries are unsigned: macOS Gatekeeper and Windows SmartScreen will
  both object until they are.

## License

MIT.
