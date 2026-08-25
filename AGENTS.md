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
  Knows nothing about how anything is drawn, and nothing about IPC either —
  the daemon translates requests onto its methods.
- **oxidezap-ipc**: the wire protocol between the daemon and its front ends,
  plus the blocking client end of the transport (`Endpoint`). No runtime: a
  front end needs one thread to read and a lock to serialize writes, and the
  daemon is the side with thousands of things happening at once. The domain
  types in `oxidezap-core` *are* the wire format; this crate adds the framing
  around them.
- **oxidezap-daemon**: binary `oxidezapd`. The only process that opens the
  store or holds a WhatsApp connection. Serves front ends over a per-user Unix
  socket and carries a tray presence.
- **oxidezap-gui**: GPUI front end, binary `oxidezap`. Talks to the daemon and
  starts one if none is listening. Owns video decode, which writes straight
  into `gpui::RenderImage` and is not reusable off GPUI.

A front end depends on ipc/core/audio and never on session: there is exactly
one WhatsApp session per user, and it lives in the daemon.

## Build & verify

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # what CI enforces
cargo test --workspace

# Running it: two binaries, and the window looks for the daemon beside itself.
cargo build --release --bin oxidezap --bin oxidezapd && ./target/release/oxidezap
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

- **The platform split lives in exactly two files.** `ipc/endpoint.rs` is the
  client end and `daemon/listener.rs` is the server end; everything above them
  — framing, requests, the whole protocol — is written once. A Unix socket is
  a filesystem entry that survives a crash and a named pipe is a name that
  does not, which is why reclaiming a stale endpoint exists on one and not the
  other.
- **Nothing stops the daemon but `main`.** The tray's Quit and an IPC
  `Shutdown` ask through `shutdown::request`; ending the process from a D-Bus
  callback or a connection task would skip disconnecting the session and
  closing SQLite. A signal would have been the obvious carrier and is not one
  Windows has, so the signal handlers feed the same in-process notification
  rather than being a second route.
- **The front end owns no session.** `oxidezap` starts `oxidezapd` when none is
  listening and speaks to it; the two ship together and the release packages
  them in one directory. A front end that cannot reach the daemon has no
  fallback, by design — a second session on the same store is the thing the
  split exists to prevent.
- **Calls ring in the daemon.** `oxidezap-session` is what opens the mic and
  speaker, so the process that owns the session owns the audio device. That
  follows from the split rather than being chosen, and it is why a call still
  works with the window closed.
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
- **The daemon's state version is what makes a mid-stream join safe.** The
  server subscribes and then snapshots, so the window between the two is
  delivered twice rather than lost, and the client drops the overlap by
  comparing versions. Reversing the order loses it instead.
- **A daemon chat that only ever arrived live is not prunable.** A complete
  store reload is the store's whole truth *about rows it has*, and during
  pairing it has none while live messages already exist. Only store-backed
  chats are diffed against a reload; see `StateHub::store_backed_chat_jids`.
- **A daemon frame is either state or news, and they use different channels.**
  State carries a version and is recoverable from a snapshot; a window request
  or a failed send is neither, so it must not ride a channel a client stops
  reading while it resynchronizes. `StateHub::apply` versus `StateHub::signal`.
- **A read the daemon issues has to outlive the reload already in flight.**
  The store's reloader was woken by the very message that raised the badge, so
  it still reports the old count moments later. `ReadTracker::read_through`
  suppresses exactly that window, and is spent the moment the chat advances or
  the store agrees — otherwise a deliberate unread from another device would
  be papered over too.
- **A read is bounded by what the *requester* saw**, not by what the daemon
  knows: `MarkRead` names the preview's message id, and anything else is
  refused. Not a timestamp — WhatsApp stamps to the second, so two arrivals in
  one burst compare equal and a client that saw only the first would slip
  through. A read action clears whole seconds, so an unchecked request from a
  stale client consumes arrivals nobody ever laid eyes on.
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
- **A front end cannot say what went wrong with a command.** `Accepted` means
  the session took it; per-request outcomes would need request ids on more
  than downloads. A failed send arrives as `SendFailed` against the chat, not
  against the request that caused it.

Clickable `div`s that remain are deliberate: a chat row and a media thumbnail
are surfaces, not commands, and have no semantic component to compose from.
Anything that *is* a command (call accept/decline, back) is a `Button`.
