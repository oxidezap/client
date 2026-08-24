# oxidezap

Unofficial WhatsApp client on top of [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust).

## Crates

- **oxidezap-core**: domain types (chats, messages, calls, UI events). No UI, no I/O.
- **oxidezap-audio**: capture, playback, Opus encoding, waveforms. cpal; no UI.
- **oxidezap-chat-store**: materializes the library's event stream into chats,
  messages, receipts and an FTS5 search index. Owns its schema and migrations;
  consumes only the library's public event surface. Extracted from
  whatsapp-rust, where it was application logic living in a protocol repo.
- **oxidezap-session**: the WhatsApp connection: events, sends, store hydration.
  Knows nothing about how anything is drawn. `oxidezapd` will wrap this crate.
- **oxidezap-gui**: GPUI front end, binary `oxidezap`. Owns video decode, which
  writes straight into `gpui::RenderImage` and is not reusable off GPUI.

A front end depends on session/core/audio, never the reverse.

## Build & verify

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # what CI enforces
cargo test --workspace
```

Stable Rust. Debug builds keep gpui at opt-level 3, because without it the UI is
unusable.

## The library dependency

All eight `whatsapp-rust` crates resolve from one git source on one branch, so
`cargo update` moves them together and no two can land on incompatible
revisions. Never pin them individually by `rev`: the resulting mismatch surfaces
as "expected `Jid`, found `Jid`" and reads like a compiler bug.

Because profile settings only apply from the workspace root, the per-package
`opt-level` sweep in the library's own manifest is *not* inherited, so the release
profile here repeats it deliberately.

## Gotchas

- **Logout is not a disconnect.** A server 401 means the stored credentials are
  dead; reconnecting with them loops forever. `AppState::LoggedOut` exists to
  force the only real recovery: wipe local state, pair again.
- **The store is one file.** Device identity, Signal state and chat history all
  live in the same SQLite database, and chat rows are keyed by device id. A
  partial wipe orphans everything behind the new device, so
  `wipe_local_state` deletes the file (plus `-wal`/`-shm`).
- **Decoded images are cached by message id**, because GPUI tracks animation
  state per `Arc<Image>` and rebuilding one re-decodes the bytes. Whoever
  replaces a preview with real bytes must evict the entry.
- **The chat store's writer queue is ordered on purpose.** Anything that
  targets a row (an ack, a nack, a local send failure) goes through the same
  queue as the write that created it, so it cannot outrun its target. A row
  past PENDING already has a real server answer and must never be regressed.
- **An invalidation is a claim that something changed.** A subscriber answers
  `StoreChange` by re-querying, so emitting one for a batch that wrote nothing
  — a receipt repeated by another of the peer's devices, a nack against a row
  already acked — buys a reload for nothing. The reload is scoped to what the
  window named: `Messages` rebuilds those chats (and their PN/LID aliases),
  anything else rebuilds the whole list, which is the only load that may prune.
- **SQLite is bundled and trimmed** in `.cargo/config.toml`. FTS5 must stay:
  the `search` feature builds its index on it.
- **No real PII in tests**, including fixtures derived from captures.

## Theme

Colours come from `cx.theme()`. The palette is registered once in `theme.rs`
into gpui-component's `Theme` global, so our surfaces and the library's own
controls resolve the same tokens. A literal colour in a component is invisible
to theme switching and drifts the moment either side changes. The two
exceptions are message bubbles (`theme::brand`, which encode authorship and
have no semantic token) and text drawn on the QR code's white raster.

Render helpers take `&App` and return `impl IntoElement + use<>`: they read
colours out of the theme but retain nothing borrowed, and without `use<>` the
2024 capture rules would make them inherit the lifetime, which the virtual
list's `&mut Context` closure rejects.

## Still to do

- **Spacing is still absolute.** ~28 `px(...)` literals where the guides want
  the rem scale (`p_2`, `gap_3`), so the UI does not respond to base-font zoom.
- **`WhatsAppApp` still owns all state**, though it is now split across
  `app/{events,recording,calls_ctl,media_ctl}.rs` rather than one file. The
  guides want per-feature entities; that is a bigger change than moving code.
- **Two large files outside the GUI**: `session/whatsapp.rs` (~2.3k) and
  `chat-store/store.rs` (~3.1k).

Clickable `div`s that remain are deliberate: a chat row and a media thumbnail
are surfaces, not commands, and have no semantic component to compose from.
Anything that *is* a command (call accept/decline, back) is a `Button`.
