# oxidezap

Unofficial WhatsApp client on top of [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust).

## Crates

- **oxidezap-core**: domain types (chats, messages, calls, UI events). No UI, no I/O.
- **oxidezap-audio**: capture, playback, Opus encoding, waveforms. cpal; no UI.
- **oxidezap-chat-store**: materializes the library's event stream into chats,
  messages, receipts and an FTS5 search index. Owns its schema and migrations;
  consumes only the library's public event surface. Extracted from
  whatsapp-rust, where it was application logic living in a protocol repo.
- **oxidezap-video**: camera capture and H.264 encoding for calls. cpal's
  opposite number: a capture backend per platform behind one crate, and the
  encoder the GUI already decodes with. No UI, and no decode — decoding
  belongs to whoever draws.
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
- **oxidezap-plugin-abi**: the wasm ABI — its constants and the widget-tree
  codec. No dependencies and `no_std`, because it is compiled into the daemon
  *and* into every plugin, including ones with no allocator.
- **oxidezap-plugin-host**: runs `.wasm` plugins inside the daemon. Discovery,
  the sandbox, and the host half of the ABI. One OS thread, one wasmi `Store`
  and one bounded queue per plugin.
- **oxidezap-plugin**: the Rust SDK a plugin is written against. Not a
  dependency of anything here; it exists to be built for wasm32.
- **oxidezap-gui**: GPUI front end, binary `oxidezap`. Talks to the daemon and
  starts one if none is listening. Owns video decode, which writes straight
  into `gpui::RenderImage` and is not reusable off GPUI.

A front end depends on ipc/core/audio and never on session: there is exactly
one WhatsApp session per user, and it lives in the daemon.

`examples/` holds plugins, and is excluded from the workspace: they build for
`wasm32-unknown-unknown` and link imports only the daemon provides, so a
`cargo build` at the root would try to link them for the host.

## Build & verify

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # what CI enforces
cargo test --workspace

# Running it: two binaries, and the window looks for the daemon beside itself.
cargo build --release --bin oxidezap --bin oxidezapd && ./target/release/oxidezap

# A plugin. Its own workspace, its own target, and the file's name is its id.
cd examples/autoreply && cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/autoreply.wasm ~/.local/share/oxidezap/plugins/
# And the one test that exercises the real SDK against the real host:
cargo test -p oxidezap-plugin-host -- --ignored
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

- **The platform split lives in exactly two places.** `ipc/endpoint.rs` is the
  client end and `daemon/listener/` is the server end; everything above them
  — framing, requests, the whole protocol — is written once. A Unix socket is
  a filesystem entry that survives a crash and a named pipe is a name that
  does not, which is why reclaiming a stale endpoint exists on one and not the
  other — and why the Windows listener builds a security descriptor by hand,
  since a named pipe's default grants read access to `Everyone` while a Unix
  socket inherits a `0700` directory. A client checks who answered, too: the
  socket sits at a predictable path, and under the `/tmp` fallback another
  user can get there first.
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
  split exists to prevent. The reach goes both ways: closing the window ends
  the front end and leaves the daemon holding the account, so the tray's Open
  has nobody to relay `ShowWindow` to and starts one instead (`daemon/window.rs`,
  the mirror of `session::connect_or_start`, down to looking beside its own
  binary first). Asking first and launching only what nobody answered is what
  keeps it from opening a second window over a live one — and who *has* one
  is said in the hello (`has_window`), not counted off the signal channel:
  every client reads that channel, so a TUI or a notifier would otherwise
  stand in for a window that is not there. The one front end name the daemon
  has to know is also the one thing worth overriding, so `OXIDEZAP_FRONT_END`
  names another — a TUI, a second GUI — and the shipped pair is only the
  default.
- **Calls ring in the daemon.** `oxidezap-session` is what opens the mic and
  speaker, so the process that owns the session owns the audio device. That
  follows from the split rather than being chosen, and it is why a call still
  works with the window closed.
- **A plugin is a front end that does not draw, and it runs in the daemon.**
  It sees the account's events and acts through the same command channel a
  window's requests go onto, so it has no privileged path to the session. It
  lives inside the daemon rather than behind the socket because the daemon is
  the only process holding the session, and wasm already supplies the
  isolation a process boundary would have been for — what wasm does *not*
  supply is a bound on time and on memory, which is why fuel metering and the
  resource limiter are not optional: a plugin that loops forever runs out and
  traps, and the daemon loses a plugin rather than a thread. The limiter
  bounds tables and instance counts and not only the linear memory's bytes,
  because a declared table is allocated at instantiation — before a
  fuel-metered instruction has run — so a byte cap alone is a bound on one
  allocation rather than on the plugin. Two allocations sit outside the
  limiter entirely and are bounded before they happen: the module's own bytes
  and whatever parsing them costs, which are spent before the store exists
  (`MAX_MODULE_BYTES`, asked of the file rather than of its contents), and the
  strings an event handle clones into the *host* (`MAX_HANDLES`) — a plugin
  asking for one list element until its fuel runs out would otherwise grow
  the daemon by far more than the sandbox advertises. A `Store` is not
  shareable and a wasm call is synchronous and blocking, so each plugin gets
  an OS thread of its own rather than a runtime task, which would stall the
  accept loop for as long as it ran. wasmi and not wasmtime: no JIT, so
  nothing generates code inside the process that holds the account, and no
  component model, which is the trade the ABI is built around.
- **A plugin's whole outside world is the `oxidezap` import module.** There is
  no WASI — not a restricted one, none — so a `.wasm` a user downloaded cannot
  read the disk or open a socket because no function exists that would. That
  is a structural guarantee rather than a policy, and it is the reason the ABI
  has no `oxi_http_fetch`: adding one turns that sentence into a promise about
  configuration, and half the interesting plugins want it, which is exactly
  why it deserves to be decided on its own rather than as a nineteenth import.
  What a plugin *may do* is a mask it declares during `oxi_init` and only
  then, because that list is what a user is shown before deciding — one that
  could widen it afterwards would make the sentence stop being true.
- **Asking is not being allowed.** Declaring a capability grants nothing;
  dropping a `.wasm` in a folder is not consent. What acts on the *account* —
  sending, marking read, showing a typing indicator — is withheld until
  somebody says yes, and the answer is recorded against the exact mask it
  answered: a plugin that comes back wanting more is not partly approved, it
  is unapproved again, because the sentence agreed to is no longer the
  sentence being asked. The mask is read before the plugin runs a single
  instruction and every check reads it live, because `oxi_init` is code the
  plugin chose too and granting for the length of one call is granting — and
  because withdrawing has to bite *now*: an answer queued behind a backlog
  would let a plugin send through five hundred banked events while Settings
  already read "not allowed", and the plugin that most needs stopping is the
  one whose queue is full. Declaring is a single act, once, for the same
  reason: a plugin that declares the narrow mask it was approved for, sends,
  and *then* widens has already sent, and the wider surface reading as
  unapproved afterwards is no use to the message. Nor does any of it start at
  instantiation — a start section and `oxi_abi_version` are code the loader
  has not accepted yet, so every import refuses until the module is
  instantiated, its version answered and its exports found. Otherwise a
  module the loader was about to turn away could send a message on its way
  out. Nor may it act on the account during `oxi_init` at all: plugins load
  before the task that consumes the command channel exists, so a send there
  would park the loading thread inside the async runtime — where blocking is
  a panic — waiting for an answer nothing can produce, and there is no
  session connected to give one. It is refused as `STATE` rather than
  `DENIED`, which says which: too early, not disallowed. What a plugin does
  only to itself — draw, keep its own settings,
  run its own timer — takes effect on declaration, and has to: a plugin that
  could not publish its settings panel before being allowed would leave the
  user agreeing to a name and a list of phrases with nothing to look at. The
  answer travels as `ClientRequest::PluginApproval` rather than a reserved
  widget id, because an id comes from the plugin's own tree — one could
  publish a button labelled "OK" carrying that id and be granted by somebody
  pressing the wrong thing. And a front end draws that switch only where there
  is something to withhold: over a plugin that wants nothing but to draw, it
  could be turned off and would read as on again, which is why
  `PluginSurface` carries `gated` beside `capabilities` — two sentences, one
  of them a question. And the file lives beside the plugins in a *persistent*
  directory, never in the plugin's own key-value store and never in the
  daemon's `state_dir`: a plugin that can write its own approval has none,
  and an answer under `XDG_RUNTIME_DIR` is one the next login throws away.
  The two share a directory, so a plugin's own store is written under a
  `kv-` prefix no plugin id can produce — one called `approvals` would
  otherwise write its settings over everybody's permissions.
- **An event is a handle, not a payload.** Nothing is serialized for a plugin:
  it reads fields through four host functions against a table of constants, so
  a handler that looks at the text and the chat pays for two strings out of an
  event carrying a dozen, and the whole path is cheaper than the JSON one a
  socket front end already uses. Field ids are constants rather than one
  accessor each, which is what keeps the import surface fixed as the table
  grows: an absent field reads back as its default — the same rule the wire
  holds itself to with `skip_serializing_if` — so adding one is a non-event
  for a plugin built against an older table. Commands go the other way as one
  import each rather than one `oxi_request` taking a serialized
  `ClientRequest`, which is what spares a plugin from carrying an encoder at
  all; the one payload that *does* travel from a plugin is its widget tree,
  and that has a fixed-width encoding written into a buffer the plugin already
  owns. A plugin needs no allocator, and `examples/autoreply` is 6 KiB.
- **A plugin's queue overflowing stops it; it does not skip.** The opposite of
  the video path, and deliberately: a frame that cannot be delivered now is
  worth nothing later, but a plugin's whole contract is having *seen* the
  messages. An autoreply that answered some people and not others, with
  nothing anywhere saying which, is worse than one that is off with a reason
  attached. "Stopped" also has to mean it runs no more of them — the worker
  checks before every event and `offer` refuses to queue another, or a plugin
  would go on working through five hundred banked messages while Settings
  reported it as stopped. A trap ends it the same way and for the same reason
  — fuel gone, memory refused, or the plugin running off the end of its own
  logic, none of which the next event improves — and it is never restarted in
  a loop, which would spend a CPU rediscovering that. Its widgets stay on
  screen, drawn inert beside the reason: a control that vanished tells nobody
  anything.
- **Stopping a plugin is dropping its channel, never queueing a message.** A
  stop message has to *fit*, and the plugin that most needs stopping is the
  one whose queue is full — `try_send` there drops the request on the floor
  and the daemon then waits forever to join a thread nobody told to leave. So
  shutdown raises a flag and drops the sender: the flag is what makes a worker
  abandon a backlog it has already been handed, and the closed channel is what
  wakes one parked in `recv`. Neither alone is enough. The bridge has the
  mirror of it: the command receiver is dropped *before* the plugins are
  joined, because a plugin parked on a command's answer is parked on a loop
  that has already stopped running — dropping the receiver drops the reply
  channel with it and the wait returns, where joining first would have the
  teardown waiting for a thread waiting for the teardown.
- **A plugin's interface is daemon state.** The plugin runs in the daemon and
  the widgets are drawn in the window, which are two processes; the answer is
  not a channel between them. A plugin *declares* a small tree pinned to a
  named slot, the tree goes into `StateHub` like everything else, and the
  press comes back as one more `ClientRequest`. So it survives the window
  closing and reappears in the next window's snapshot, because it was never
  the window's in the first place — and a front end that is not a window reads
  the same tree and renders it its own way or ignores it. A slot is a promise
  about *where*, never about how: nothing in a tree can express a colour, a
  size or a position, so a plugin cannot put a literal outside the theme's
  reach. The open chat travels on the action rather than being looked up,
  because the daemon does not know it — two windows can have different
  conversations open, and a header button is about the one the person pressing
  it was looking at.
- **The camera is where the microphone is, and the picture crosses encoded.**
  `oxidezap-session` opens both, because the process that owns the session
  owns the devices — so the window has no camera of its own and no way to
  draw what it is sending. What crosses the socket is therefore *both*
  directions of the call, as H.264 access units: 16 KiB a frame against 3.5
  MiB of pixels, and the front end already carries a decoder for the video it
  plays in a conversation. Sending the self-view as the very stream the peer
  receives costs one more decode and no second encode, and is the only form
  of it that cannot lie about what they are seeing. Frames are a third kind
  of daemon frame beside state and news (`StateHub::publish_video`), because
  they obey neither's rules: no version, nothing recovers them, and a client
  that falls behind is *right* to skip — sharing the session channel would
  turn a slow window into a `Resync` and throw its history away to catch up
  on a picture that had already moved on. It is gated on `has_window` rather
  than on wanting events: a notifier asks for events and has nowhere to put a
  picture, and subscribing it would spend a call's whole bitrate on frames it
  parses and discards. And the *session* stops producing them when the last
  window goes: nothing announces a subscriber leaving, so the first frame
  that finds nobody drawing is what notices, and `set_video_publishing`
  closes the door in front of the sender until a window subscribes again. The
  gate is read before a frame is built, because building one copies an access
  unit out of the encoder's buffer — for a call that runs, and a peer that is
  receiving it, whether or not anybody here is looking.
- **Everything on the video path drops, and every drop asks for a keyframe.**
  A frame that cannot be delivered now is worth nothing later, so every queue
  from the encoder to the pane is short and every send is a `try_send`. What
  a drop costs is the reference chain — each unit after it points at one the
  far side never received — so the sender's queue asks its own encoder for an
  IDR, the peer's RTCP PLI asks for one through `CallEvent::RtcpReceived`,
  and the window's decoder, which can ask nobody, waits for the next one
  rather than rendering a second of torn macroblocks over the last good
  picture. Every moment a decoder is *born* mid-stream is asked for too — an
  outgoing call renamed off its placeholder, one the peer has just answered,
  and every camera that becomes drawable, since the encoder opened before the
  offer or the announcement did and its opening IDR was published nowhere.
  Without that ask the first frame a new decoder sees is a P-frame and the
  pane says "connecting" until the periodic IDR, seconds later.
- **A peer's parameter set is read before a decoder sees it.** A decoder
  allocates its reference and output buffers from the SPS — from numbers the
  person on the other end of the call chose — so a pixel budget applied to
  the decoded picture is applied after the allocation it exists to prevent.
  `video::sps::coded_size` reads the geometry out of the access unit first —
  out of *every* parameter set in it, answering the largest, because one unit
  may carry several and the slice picks which one it is coded against: a
  thumbnail-sized set in front of the one the picture really uses is a budget
  walked straight past. It answers `None` for a unit with no parameter set
  (nothing new is being declared) and for ones it cannot follow, which is
  deliberate: refusing on a reading nobody has checked would break a
  legitimate call over a parser bug.
- **A decoded picture is a slot, not a place in a queue.** The window's event
  channel is hundreds of messages deep because the messages that may not be
  lost need it to be, and a decoded 720p frame is 3.5 MiB — so frames put
  there would let a stalled window bank gigabytes of obsolete video *and* park
  every state frame behind ten seconds of it. `LatestFrames` holds one picture
  per direction, the newest overwriting the last, and the channel carries only
  a nudge; a dropped nudge costs nothing, because the slot still holds the
  newest picture and the next frame nudges again.
- **A peer's orientation describes their device, not their picture.** The
  camera encodes in the sensor's orientation whatever the phone is doing, so a
  frame arrives already turned by however it is held and `device_orientation`
  is the *description* of that turn. Drawing it upright means undoing it —
  `Rotation::to_upright`, not the turn itself. Applying it again is the one
  mistake that looks deliberate: at one quarter turn it is 180° out, which
  reads as a peer standing on their head rather than as a sign error.
- **A camera is a request, not a state, and requests arrive out of order.**
  Opening one is device work — tens of milliseconds, and a permission prompt
  the first time — so two toggles spawned in order routinely start in the
  other, and `VideoLane` is the mute lane's twin for exactly that: the intent
  is stamped on the caller's thread before its task exists, the newest
  request is the only one that may speak, and what it publishes is read back
  from the registry rather than from what was asked for. A camera that will
  not open, a call hung up while it was opening, an announcement the peer
  never got, and a device unplugged mid-call all end the same way — the
  registry entry is what "our video is on" *means*, and `settle_video` says
  what is in it.
- **A refusal is answered by whether one is outstanding, not by which camera
  asked.** The library does not match a refused upgrade to the request it
  refuses: its handler tears the local plane down whenever *some* request of
  ours is pending — whichever camera is attached by then — and ignores the
  stanza when none is. So `CallRegistry::upgrading` holds presence rather
  than identity, and the camera goes off exactly when the library has
  released its endpoints. Keying it on the camera the request went out with
  reads as more careful and is worse: a refusal landing after an off-and-on
  again tears down the replacement's plane in the library while leaving it
  registered here, drawn as live, encoding into nothing. The presence is
  stamped *before* the request goes out, for the reason every intent here is
  stamped before its task exists: the reply is not ours to schedule, and a
  peer refusing while `start_video` is still awaiting would otherwise find
  nothing outstanding and leave the camera standing over a plane the library
  has already released. Registering early is the half that can be made safe —
  every path out that is not a camera held withdraws it again, and the
  refusal's own teardown queues on the call's video lane behind the enable it
  is answering.
- **What a call turned out to be is said by the side that opened the
  device.** The kind is drawn from the offer, because that is all anyone
  knows when the call is placed or answered — and a camera that will not open
  downgrades it to voice rather than failing it, on both paths. So
  `OutgoingCallStarted::is_video` carries what the offer actually went out
  as, and `UiEvent::CallAnswered` what the accept actually attached; without
  them a window holds a video layout open on a call with no picture in it and
  the conversation records a video call that never was one.
- **A video call is offered as one, and answered as one.** The endpoints have
  to be attached before the offer or the accept goes out, which is why the
  camera opens first and why a camera that fails downgrades the call to voice
  rather than failing it. It is also why the daemon reads `is_video` off the
  ringing offer rather than taking a front end's word: the library refuses
  `.video()` on an audio offer. The peer's mid-call request to add video gets
  no dialog of its own — turning our camera on *is* the acceptance, and the
  token that binds it to that request never leaves the session — but the
  *question* is state (`CallVideo::requested`) rather than one window's
  memory of an event: a window that attached after it was asked never saw the
  event, and would draw an ordinary camera button while somebody waited on
  it. It clears when a camera comes on, which is the answer, and when the
  peer withdraws it.
- **Ending a call is something you say, and muting is something you may fail
  to say.** A hangup is `CallHandle::terminate`, which sends `<terminate>` to
  every device a still-ringing call rang and then tears the local side down
  whatever the stanzas did — `hangup_local` is for the one case where the peer
  already knows, their own `<terminate>` arriving. Getting that backwards is
  what left a cancelled outgoing call ringing at the far end until its
  transport gave up. Mute is the mirror image: the library commits the two
  directions *around* the `<mute_v2>` — a mute applies before it, an unmute
  only once it is out — so the microphone is never live while the peer is
  shown a muted one, and the price is that a failed announcement leaves the
  device in a state the front end did not ask for and has already drawn.
  `set_call_muted` asks the handle what it really holds and publishes it as
  `UiEvent::CallMuteChanged` — always, not only when it differs from what was
  asked. Two things make the state honest and neither is the comparison: the
  request is stamped on the caller's thread before its task exists, because
  spawning is not sequencing and the superseded half of a rapid toggle must
  stay silent rather than restore its own value; and the newest request
  speaks after it has reached the device, so it is the last word. A word said
  only on disagreement is unversioned, and would let a failed announcement's
  answer stand over the success that came after it. Agreement costs nothing,
  because a call state that does not change sends no frame.
- **Logout is not a disconnect.** A server 401 means the stored credentials are
  dead; reconnecting with them loops forever. `AppState::LoggedOut` exists to
  force the only real recovery: wipe local state, pair again.
- **The store is one file.** Device identity, Signal state and chat history all
  live in the same SQLite database, and chat rows are keyed by device id. A
  partial wipe orphans everything behind the new device, so
  `wipe_local_state` deletes the file (plus `-wal`/`-shm`).
- **A call's pictures are the call's, and the state says which are live.**
  `CallVideo` is two independent flags because either side may turn its
  camera on and off mid-call, and a call where only one is on is the ordinary
  case. A pane draws the newest frame it has and the *state* decides whether
  it draws at all: a camera switched off simply stops sending, so a pane left
  holding its last frame is a photograph of somebody who has gone. Frames and
  state travel on different channels, so both ends check both — a frame for a
  call that has ended would put the last person's face on this one, and a
  frame for a direction just turned off would light a pane nothing will come
  to clear again.
- **Decoded images are cached by message id**, because GPUI tracks animation
  state per `Arc<Image>` and rebuilding one re-decodes the bytes. Whoever
  replaces a preview with real bytes must evict the entry.
- **The daemon's state version is what makes a mid-stream join safe.** The
  server subscribes and then snapshots, so the window between the two is
  delivered twice rather than lost, and the client drops the overlap by
  comparing versions. Reversing the order loses it instead. The snapshot is
  also the *first frame*: a summary carries everything a chat row draws, so
  `catch_up` turns the list into the load event a front end already handles,
  and a window opens with the chats in it rather than flashing them in when
  its own store load returns. Never `complete` — a summary has no messages, so
  it may not prune — but store-backed, because these rows *are* the daemon's
  list and the daemon's list is the store's, so a later complete load is
  allowed to contradict them. Which is also why none are sent while pairing:
  there the store is empty and whatever the daemon holds arrived live. They
  stop at the window the session's own load fills, because a row past it is one
  no load will ever put messages in. And a row without messages cannot be read:
  `MarkRead` names what the requester saw, the daemon refuses one that names
  nothing while it knows a boundary, so opening such a row banks the read
  (`owed_reads`) and the load that brings the messages spends it — otherwise
  the badge clears locally, no receipt goes out, and the next hydration puts it
  straight back.
- **The status reader is anchored to an update, not to a place in the run.**
  A position was safe only while a run grew at the end, and it does not: a
  live update and a hydrated one can both be stamped before the one being
  watched, and the same index then silently becomes a different message —
  never marked watched, never fetched, with the previous one's video still
  playing over it. `StatusPane::shown` is the anchor and
  `reconcile_status_pane` puts the index back under it.
- **A daemon chat that only ever arrived live is not prunable.** A complete
  store reload is the store's whole truth *about rows it has*, and during
  pairing it has none while live messages already exist. Only store-backed
  chats are diffed against a reload; see `StateHub::store_backed_chat_jids`.
  On the window's side the same diff spares what is *on screen* rather than
  what is selected — the selection survives a trip to Status, to Settings and
  under the viewer, so sparing on it kept a deleted chat nobody was looking
  at. `departed_chats` is the deferral and the render pass spends it, against
  what the previous frame drew.
- **How a call ended is said in the state, not derived from its absence.** A
  front end learns a call is over by watching the stage disappear, and it
  writes the conversation's record from the stage it was holding — so a call
  answered on another device reads as missed, one the daemon refused to place
  reads as an attempt that was never made, and one *another window* declined
  reads as missed in every window but that one. `CallState::ending` is the one
  answer to all three: `Ending::Nothing` for the calls with no honest local
  record, `Ending::As` for an outcome only the acting side knew. It travels in
  the same frame as the removal, because an explanation sent beside it rides a
  different channel and can arrive after the record it was meant to change.
- **An outgoing call is named twice, and the second name lands by the first.**
  The window draws the call it placed before the server has answered, under a
  placeholder id of its own; `OutgoingCallStarted` carries both, and the rename
  is matched on the placeholder. Matching on the recipient instead was right
  until someone gave up and dialled again: the abandoned attempt's answer then
  renamed the *redial*, so the state held an id nobody was ringing under and
  the window's orphan-cancellation path let the abandoned call ring on.
- **An account reset is a departure, not just a clear.** Everything a
  disconnect stops has to stop here too — `forget_account_state` goes through
  `leave_connected_view` — and everything keyed to the account has to go,
  including the two selections that are JIDs themselves (the status reader and
  the destination) and the call state. A stage left standing is read as ending
  by the *next* account's first snapshot, which writes the old peer's call into
  the new account's history.
- **The call card belongs to the window, not to the conversation.** It is
  drawn by the root, above whichever screen is up, because a call arriving
  while Settings was open rang at the far end with no card, no Accept and no
  Decline anywhere — the card and `sync_overlay_focus` were both built by the
  conversation view alone.
- **Whenever the stage empties, the parked caller comes forward.** A second
  offer during a call waits behind the one on screen, and nothing draws a
  waiting call on its own — so a stage cleared without promoting it leaves
  someone ringing with no card, no Accept and no Decline. The rule is about
  the stage being empty rather than about how it emptied, which is why
  `CallState::promote_waiting` is one method that `take`, `end` and
  `fail_outgoing_to` all go through, and why `take_incoming`/`take_outgoing`
  deliberately do not: those hand the stage to what replaces it.
- **Watching a status is the row's own ack, not a second place to look.**
  There is no receipt to send — a status read receipt is a privacy setting the
  library does not expose — and the broadcast's unread cursor cannot say it
  either: that counter covers one chat holding *everybody's* updates, so
  clearing it would watch every contact's run at once. It goes where WhatsApp
  Web puts it, on the message: `messages.status` moved to `Read`. That column
  is inert on an incoming row — written once at insert as `Delivered`, and
  `advance_status` only ever moves `from_me` rows — so `Read` there has one
  meaning. It goes through the writer queue like every other write that
  targets a row, which is also what invalidates the broadcast: the reload that
  follows is how every *other* window learns, over the channel it can already
  recover from, rather than a piece of news a lagging client would miss. A
  window still remembers its own views, but only until the load that carries
  them proves the store agrees — a claim nobody else disputes is not one worth
  holding. And a refused view does not force the ring back on: the flush
  contract is temporal, so a refusal is not proof that nothing was written, and
  the only honest answer to "did that land" is to read the history again. It
  also means the broadcast's own unread counter never comes down, which is why
  nothing totals it: the tray's badge and `StateSnapshot::total_unread` both go
  through `ChatSummary::counts_toward_unread`, or the tray claims unread
  messages over a chat list with nothing unread in it and no way to clear them.
- **A revoked message is a fact, not a sentence.** The store keeps the row
  and hydration turns it into "[Message deleted]" — which a conversation is
  right to draw and the status feed is not: an update its author took back has
  nothing left to watch, and counting it kept a ring and a badge up for the
  rest of its 24 hours. `ChatMessage::revoked` is what the feed asks, so
  nobody has to recognise the text.
- **A transient surface that takes the keyboard has to give it back, and to
  one place.** The call card's Enter and Escape and the viewer's arrow keys
  are scoped to their key contexts, so they do nothing unless something
  focuses them — and a teardown that merely blurs leaves the window with no
  keyboard target at all. `KeyboardOwner` names who should have it and
  `sync_overlay_focus` hands it over, from the render pass, because focusing
  needs a `Window` and the state it follows comes from the daemon. A ringing
  call outranks the viewer; an *answered* call owns nothing, because a call
  people talk through is one they type through — which is why mute is a
  window-wide chord rather than a card binding. The list ends in the window
  itself, and that end is what makes the rule total: focus may only be put on
  a handle the frame actually drew — an absent one sends every key to gpui's
  own root, past every handler we hung off ours — so the surfaces name
  themselves per frame (`KeyboardSurfaces`) and the root's own handle is what
  remains when none of them is drawn. There used to be no such floor: the
  owner was recorded as the composer before a composer existed, the first
  sync found nothing to change, and every window-level shortcut stayed dead
  until a click gave the window a focus of its own. On a desktop that click
  happens in the first seconds; on a handheld with no pointer it never
  happens, and the window never listens. The same rule binds the commands
  that move focus themselves: `focus_search` and `open_settings` reach their
  surface by *navigating* to it — out of Settings, off Status, back to the
  list on a phone — and refuse outright where there is nowhere to navigate
  to, because a shortcut that focuses something the screen does not draw
  leaves the window as deaf as having no focus at all. Where two surfaces are
  both drawn the gesture decides, not the ordering: `ChatOpen` already says
  whether a chat was opened to be talked to or looked at, and a composer that
  took the keyboard on selection ended a keyboard walk through the list after
  one step.
- **What a recording will be sent as is bound when the microphone opens.**
  Not read when it closes: the destination *and* the reply it answers are one
  answer to "where is this note going", and resolving either at the end sent
  it to whichever chat was on screen by then, or quoted whichever message had
  been picked since. `RecordingTarget` is that pair, and the draft is cleared
  at send only if it is still the one the note was bound to.
- **An overlay that names a row is reconciled where rows change.** The media
  viewer holds a message id and resolves it every frame, so a revoke behind
  it left a modal that drew nothing and still swallowed the Escape meant to
  close it. `invalidate_message_cache` is the announcement that a chat's
  history changed, which makes it the whole set of ways the thing being
  looked at can stop existing.
- **The media directory holds two different things.** `f-`/`d-` is the
  cache — bytes the daemon fetched and can fetch again — and `u-` is a payload
  a front end staged for a send that has not run yet, which is its only copy.
  `Wipe::Cache` is what "clear cached media" may take; `Wipe::Everything` is
  for the account leaving. A writer that cannot be cancelled asks
  `media::epoch` instead: the eager cache of an inbound message loses to a
  clear, and a download somebody asked for does not, because there the file is
  how the bytes are delivered rather than where they are remembered.
- **Nothing may still be writing this account's media when it is deleted.**
  The publish thread externalizes media behind an unbounded queue, so an
  event accepted before `ForgetSession` can still be in it. `stop_publishing`
  closes the queue and hands back the thread to join, before the wipe.
- **The timeline anchor describes the rows, not how many there are.** The
  list keeps a measured height per index, so the only question worth asking
  is whether the rows it measured are still those rows. A count cannot say:
  a backfill before the head, a notice stamped in the past and a message
  landing mid-history all raise it exactly as an arrival does, and only an
  arrival leaves the earlier rows alone. The row at the end of the measured
  prefix is what answers it — and the honest form of that question is which
  of the rows it measured this frame still draws, and where
  (`MessageListCache::common_prefix`/`common_suffix`): what they share at
  either end is what may be kept, and the stretch between is one splice,
  removal and insertion alike. Neither end is where a count would put it. The
  encryption notice holds index 0 whatever arrives in front of the messages,
  so a page of older history is an insertion in the *middle* and splicing it
  at 0 slides the notice's height onto a message; the typing indicator holds
  the last index whatever arrives behind them, so an arrival under it read as
  a page and went to the top; and a page can swallow a divider its own newest
  message now shares a day with. A row can also change height with the rows
  standing still — an image arrives, a reaction lands, a send fails and grows
  a retry button — which the `build` number answers: a rebuild with the rows
  unchanged is a remeasure, never nothing, and an unchanged build is the frame
  that keeps the diff off the hot path.
  Only another conversation resets.
- **What a frame leaves out, its reader fills in.** The wire is
  newline-delimited JSON and a history load is a hundred chats of fifty rows,
  most of whose fields are empty — no reaction, no quote, no media, nothing
  revoked. Every one of those is `#[serde(default, skip_serializing_if …)]`,
  which is about a third of the frame in bytes and in the two serde passes
  over it. The pairing is the contract: a field may only be skipped where its
  absence reads back as the value that was skipped, which
  `an_omitted_field_comes_back_as_what_it_was` is there to hold. It is also
  why these types travel one way only — nothing in `ClientRequest` carries a
  `ChatMessage` — so a sparse frame is never handed to an older reader.
- **An answer nobody delivered is a request nobody answered.** A connection's
  outbox is bounded, and a page or a download dropped into a full one leaves
  the asking view waiting on it forever — the front end keeps the request in
  `pending` and its list never asks again. Nothing may block on that queue
  either, because the caller is the bridge and the session waits on it, so
  `answer_now` hands a full outbox to a task that waits on the connection's
  own writer. A frame is dropped only when the connection is gone.
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
  knows: `MarkRead` names a message id, and one from an older second is
  refused. A read action clears whole seconds, so an unchecked request from a
  stale client consumes arrivals nobody ever laid eyes on. What it may *not*
  demand is that the id be the daemon's own newest: WhatsApp stamps to the
  second, the store returns a burst in arrival order and a front end sorts it
  by `(timestamp, id)`, so `messages.last()` names a different message on each
  side. Requiring them to match refused every read of a chat that had ever
  received two messages in one second — permanently, since asking again
  produced the same id. Membership in the boundary second is the test, and
  either half of a burst is an honest claim to have seen it.
- **A person has one name, and one place decides it.** WhatsApp answers "who
  is this" three ways — the synced address book, the push name the sender
  chose, the number — and the live path used to ship the push name while a
  hydrated row resolved the address book, so the same participant was one
  name on their bubbles and another on the typing line above them.
  `session/names.rs` is that choice made once, in one order, for live
  messages, chat presence and hydration alike; `Chat::update_participant` is
  the one place a name enters a conversation and writes it onto the rows that
  were waiting for it, and `Chat::author_name` is what every surface asks. A
  full history load is what re-reads the address book, so it is also what
  clears the book's memo. It decides the *key* as well as the label:
  `ChatIdentity::canonical_jid` is what a person is filed under, so a
  composing arriving as a phone number and its paused as a LID do not become
  two entries — one of which nobody can clear, leaving the typing line up
  until its TTL runs out.
- **A history load is read in pages, not in rows.** Every pooled read costs a
  permit, a blocking task and a snapshot transaction before it runs anything,
  so a query per message multiplies all of that by the size of the account:
  the hundred chats of fifty messages an attaching front end asks for came to
  five thousand reads, most of them spent learning that a message has no
  reactions. `ChatStore::reactions_for` and `ChatStore::pages` are the batch
  shapes, and the single-row `reactions` is a page of one so there is one
  statement to keep right. `pages` takes a limit *per chat* rather than one
  for all of them, because a load that serves a chat list wants the newest row
  of most chats and the unread tail of a few. Measured by
  `history_hydration_costs`, which is ignored by default because it is a
  stopwatch.
- **History is asked for, not pushed.** The attach load carries the chat list
  and, per chat, only what the *daemon* needs of it: the newest row the list
  previews from and the unread tail, which is the set of receipts a read owes
  and the second it is bounded by (`attach_page`, floored so an ordinary
  same-second burst is covered). A timeline is a page a front end asks for
  when it has somewhere to draw it — `LoadMessages` on opening a conversation
  and again as the reader nears its top, `LoadChats` as the sidebar nears its
  end. Near the top is two questions for a bottom-anchored list, because it
  has no scroll position until somebody scrolls it and answers "which row is
  at the top" with the row *past the last one* while it has none. Taken as a
  position that is the far end, so a conversation whose rows do not fill the
  window — the one with the most reason to ask — never asked, and had no
  scrollbar to say so with either: `paging::timeline_nearing_start` reads that
  second fact as the first. WhatsApp Web sizes it the same way and preloads
  neither (`web_preload_chat_messages`, `web_init_chat_batch_size`,
  `history_sync_on_demand_message_count`).
  A list that has reached its end has only reached the end of what the store
  holds *now*: a history sync commits over minutes, so `Paging::Done` keeps
  the cursor it last asked with and *any* history load reopens it
  (`reopen_finished_pages`) — not a complete one, which is a load that
  returned fewer chats than it asked for and so is a load an account of a
  hundred chats never gets — the rows that arrive are older than everything
  fetched, which is exactly where that cursor points. And an empty list is at
  its end like any other, so the frame asks on the sidebar's behalf when a
  filter matches nothing: the virtual list that would have asked is not built
  when there is nothing to put in it.
  Where the list continues is said by the load that walked it: a truncated
  `HistoryLoaded` carries the position it stopped at and a complete one is the
  whole list, so a window's first "load more" is a page it does not have —
  adopted only by a list that has not asked for anything, since that position
  is where the *first* page ends and every later load carries it again. It
  costs the load nothing — it has already walked that far — and the ask it
  replaces was a hundred rows re-read, re-serialized and re-merged to learn
  one token.
  Two rules keep it honest. A cursor is **opaque** — what a page is ordered by
  is the store's business, and a front end that parsed one would be a second
  implementation of that order — so `PageCursor` is a token the daemon writes
  and reads, and `session/whatsapp.rs` is where it is spelled. And the daemon
  **learns from what it serves**: a page of messages is folded into
  `ReadTracker` and a page of chats into the hub *and* the tracker on the way
  out, because a read is bounded by what this side has observed and a chat
  past the attach window is otherwise in no snapshot — a window naming either
  would be refused for naming something the daemon has never heard of. A page
  is a frame like any other, so its media is externalized like any other
  (`externalize_messages`) and read back on the client's own IPC thread; the
  page of chats is sized exactly as the attach load sizes one (`attach_page`),
  because a read owes a receipt per unread message rather than one for the
  chat and the status broadcast is nobody's conversation to open; and a page's
  rows carry each row's other half — the attach load's too, now that a front
  end continues past that window rather than re-fetching it — since a PN/LID
  pair is collapsed over the rows one hydration is given and a page boundary falls wherever the store's order
  puts it — half a pair alone is a chat with half the pair's unread count,
  merged over the whole one the window already had.
- **The reload debounce is for bursts, not for askers.** A history sync commits
  many batches and each emits a change; the quiet window folds them into one
  load. A front end that asked outright is not a burst — it holds nothing, it
  asked for everything, and waiting the window out is a fifth of a second
  before the first query — so `spawn_history_reloader` skips the debounce on an
  explicit ask, and watches for one *inside* the drain as well as outside it: a
  sync's changes never stop arriving, so a drain waiting on them alone has no
  quiet window to end on and the asker waits out the whole sync.
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

A scrollbar belongs to whatever scrolls, and both lists have one: the sidebar
hands `Scrollbar::vertical` its `VirtualListScrollHandle` and the conversation
hands it the `ListState` itself, since a self-measuring list is the only thing
that knows how tall its rows turned out. In both it is drawn over the scrolling
region at its trailing edge, outside the rows' own gutter — and *where* that is
comes from the handle rather than from the element the bar was hung on: a
`Scrollbar` paints itself over the bounds its handle reports, so the overlay
around it only has to exist. Which is why a gutter belongs to the rows and
never to a container wrapped around the list: padding there moves the list, and
the bar with it, leaving it hanging a gutter's width inside the pane.

## Theme

Colours come from `cx.theme()`. The palette is registered once in `theme.rs`
into gpui-component's `Theme` global, so our surfaces and the library's own
controls resolve the same tokens. A literal colour in a component is invisible
to theme switching and drifts the moment either side changes. The two
exceptions are message bubbles (`theme::brand`, which encode authorship and
have no semantic token) and text drawn on the QR code's white raster. The
fullscreen viewer's ground *was* a third, and is not: `scrim`/`on_scrim` are
its own pair of tokens, because the theme's inks are the wrong answer there —
`background` is the deepest surface in a dark preset, which is near-black
text on a near-black wash.

Render helpers take `&App` and return `impl IntoElement + use<>`: they read
colours out of the theme but retain nothing borrowed, and without `use<>` the
2024 capture rules would make them inherit the lifetime, which the virtual
list's `&mut Context` closure rejects.

## Responsiveness

One number, in one place. The window's size reaches the interface as a factor
on the base font — `theme::metrics::viewport_fit`, applied by
`theme::fit_to_viewport` from the root's render pass — and everything else
follows from the rem: type steps, vertical rhythm, control frames, the QR
code, the layout breakpoints, and the row heights the timeline has cached
(which is why the fit is quantised: it moves `Metrics::rem_size`, their
invalidation key). A component never learns that small screens exist. The
breakpoints are themselves rem-derived, because "is there room for two panes"
is a question about the content and not about the glass — the same 700px
window holds two panes at the reference base and one at double it.

Two consequences. A base font is bounded in exactly one place — `Metrics` —
so the rem handed to gpui-component's `Theme` is the one `Metrics` resolved
rather than a second multiplication beside it; the smallest configurable font
at the smallest fit lands under the floor, and two answers there put our
chrome and the library's buttons on different scales in the same header. And
row heights measured against a scale are stale when it moves: `TimelineAnchor`
carries the rem *and* the width it measured against, because the fit changes
one at a step boundary and dragging an edge changes the other.

Two things that are *not* the fit. The window opens no larger than the display
(`opening_size`), because a window that opens off the edge of a handheld is
one nobody can drag back. And a pane that centres its content must also be
able to scroll it: `views::centered_view` does both from one layout, since a
column that is only centred is clipped at *both* ends the moment it outgrows
the window — which is how a 640px-tall screen showed the middle of the pairing
screen, with the title above the glass and the pair code below it.

## Still to do

- **Spacing is still absolute.** ~28 `px(...)` literals where the guides want
  the rem scale (`p_2`, `gap_3`), so the UI does not respond to base-font zoom.
- **`WhatsAppApp` still owns all state**, though it is now split across
  `app/{events,recording,calls_ctl,media_ctl}.rs` rather than one file. The
  guides want per-feature entities; that is a bigger change than moving code.
- **Two large files outside the GUI**: `session/whatsapp.rs` (~2.3k) and
  `chat-store/store.rs` (~3.1k).
- **Group video is drawn but not reachable.** `call_card/video.rs` carries a
  participant grid the library's group calls would fill; 1:1 is what the card
  routes to today.
- **A front end cannot say what went wrong with a command.** `Accepted` means
  the session took it; per-request outcomes would need request ids on more
  than downloads. A failed send arrives as `SendFailed` against the chat, not
  against the request that caused it. A *plugin* does learn this, which is the
  odd part: its call is synchronous, so there is nothing to correlate.
- **A plugin cannot reach the network or the disk, and half the interesting
  ones want to.** A translator, a webhook bridge, a conversation export. Each
  is one import, and each turns the categorical sentence in the gotchas above
  into a policy — so it wants a declared destination, a prompt at enable time,
  and a decision of its own.
- **Plugins are not reloadable, and there is no message interception.** A
  plugin with state, reloaded under itself mid-conversation, is a separate
  problem; restarting `oxidezapd` is the answer for now and it is cheap. And a
  plugin that could alter or block an inbound message would sit between the
  store and every front end, which the whole state model assumes it cannot —
  plugins observe and act, they do not filter.

Clickable `div`s that remain are deliberate: a chat row and a media thumbnail
are surfaces, not commands, and have no semantic component to compose from.
Anything that *is* a command (call accept/decline, back) is a `Button`.
