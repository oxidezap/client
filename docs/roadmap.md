# Still to do

> Known gaps and why each is still open. **Confirm a gap is still a gap before
> acting on it** — some of these describe upstream behaviour that may have been
> fixed, and any count or file size here is from the commit that wrote the
> entry. The reasoning is the durable half.

- **Spacing is still absolute.** There are `px(...)` literals where the guides
  want the rem scale (`p_2`, `gap_3`), so that part of the UI does not respond
  to base-font zoom. Survey them before planning the work rather than trusting a
  number here, but do not take a raw `px(` count for the size of the debt: it
  also matches GPUI's `.px(...)` padding method, which is correct when it is
  passed a metric (`.px(metrics.space_xl())`), and bare `px()` is the right
  conversion for an intrinsic size that is genuinely in device pixels — decoded
  media geometry, for one. What counts is a `px()` around a *literal*, outside
  `theme/` (where the scale is defined) and outside test modules. Read the
  matches; the ones that need changing are the ones naming a number nobody
  derived.
- **`WhatsAppApp` still owns most state.** Four clusters are per-feature
  entities now — `app/{paging,recording,notices,recovery}.rs` each own a
  struct the app holds an `Entity` of, rather than fields on the root — and
  the rest is still fields on one struct, split across `app/*.rs` by file
  rather than by owner. The pattern the four settled on is in the module
  headers: the entity owns its state and the tasks that mutate it, methods
  take `Context<TheEntity>` so they *cannot* mark the app dirty, and anything
  needing a chat, a session or a draft stays above it. What is left is the
  clusters that share state — playback and status both answer "what is
  playing" — and the four `RefCell` caches, which exist because the render
  pass holds `&self`, and which an entity split is what retires.
- ~~**Two large files outside the GUI**~~ — done. `whatsapp/mod.rs` is now
  `whatsapp/{media,history,paging,lanes,convert,tests}.rs` beside a `mod.rs`
  holding the event pump, and `chat-store`'s `store.rs` is a `store/` split by
  the event kind each function materializes. Both were moves: the bodies did
  not change, which is what made them reviewable at that size.
- **The session still runs on the window's own thread.** Two of the three
  things that wanted a dedicated worker are in place: `exec::sleep` arms its
  timer on a worker global as readily as on a window, and `store/web.rs` asks
  for the OPFS handle before falling back, so both backends are written and
  the pragma and the wipe already dispatch on which answered. What is left is
  the move itself — `daemon::embedded` assembled inside a worker, with the
  front end's `Link` a `MessagePort` rather than a `tokio::io::duplex` — and
  it is the expensive half: the session, the store and the bridge all change
  address space at once, and the page that works today is the thing at risk
  if it is got wrong. `wasm_thread` is already in the tree through gpui, so
  the spawn is not the obstacle; the restructuring is.
  What it *costs* is measured now, which it was not when this was written. A
  DevTools trace of a cold load of the published page puts 93% of the
  window's whole main-thread CPU in the first three seconds, and the session's
  share of that is two blocks: 129ms, then 342ms with not one animation frame
  in it, ending at the `WebSocketCreate` that opens the socket. That second
  one is the store's preload, SQLite, the migrations, the client and the
  hydration, and it is one block because every `.await` between those phases
  is ready when it is polled — the asynchrony belongs to the desktop's
  runtime, and SQLite in a page is synchronous, so nothing there ever leaves
  the microtask it started in. `run_client` now breathes between the phases
  and times each one, which changes the shape and not the total: an `info` line
  says where a page's first second went, in a module whose symbols are
  stripped and whose flame graph therefore names nothing. A turn is an
  opportunity to draw rather than a promise of one — the browser decides
  whether a rendering pass fits between two tasks — and the total is still the
  worker's to remove.
  What a turn is *made of* is the part that took a second look. A zero-length
  `setTimeout` is the obvious yield and is the wrong one here: a browser clamps
  timers in a hidden document to about a second, and the tab holding the
  account is routinely the hidden one — that is the whole of `ipc::tab`, where
  one tab serves the others. Five yields on the cold-start path would then be
  five seconds before the account came up in the tab somebody *is* looking at,
  and five seconds is exactly `SHUTDOWN_GRACE`, so a stop arriving mid-start
  would spend the whole grace waiting for the start to reach the select that
  answers it. `exec::breathe` posts through a `MessageChannel` instead: a
  port's message is a task like a timer's callback is a task, with the same
  rendering opportunity behind it, and no clamp on the hidden document or on
  the nesting depth. The other two yields in the tree —
  `plugin_host::sched::breathe` and `gui/platform/clock.rs` — are still
  timers, and are not on a path where five of them compound; the sentence they
  share with this one is now about *why* a turn is a task, rather than about
  which call makes it.

- **The session runs in the browser, and pairing is measured now.** A page
  with no daemon named starts its own, and the whole of it works against
  WhatsApp: the VFS opens, the store and its migrations run, `ChatStore` comes
  up, the library's client dials `wss://web.whatsapp.com/ws/chat`, the QR is
  drawn, a phone scans it, and messages go out and come back. The upgrade
  succeeds from a page served off `https://oxidezap.github.io`, which is a
  public origin and not WhatsApp's own — a WebSocket upgrade is not subject to
  the same-origin policy, and the server declines to make it one.
  What stood between the handshake and the QR was `AbortHandle` — the entry in
  docs/gotchas.md on an abort being something said — and it
  is worth remembering how it looked: everything a log could show was working.
  The socket opened, the handshake completed, the server's `<pair-device>`
  arrived and was acked. Only the ack is inline; the six refs are rotated by a
  detached task, and a detached task was one this page cancelled. So the
  failure presented as a page that connects perfectly and pairs never.
  Durability is the other half. The window's VFS is relaxed-IndexedDB, which
  writes changed blocks after the commit rather than during it, so a tab killed
  in that window loses the commit — a message that comes back on the next
  hydration, or a ratchet that has to re-establish. Nor is an ordinary commit
  *observable*: the VFS answers for an import, a deletion and a clear, and
  hands back nothing for the writes a session makes — so a quota the browser
  refuses has nowhere to be reported, and the account behaves perfectly all
  session and is gone on the next load. What the store does about that is ask
  the browser to keep this origin and say the headroom out loud when it opens,
  which is a warning rather than a fix.
  The durable answer is OPFS through a synchronous access handle, and
  `prepare` asks for it *first* rather than assuming: the handle is specified
  to exist in a dedicated worker and nowhere else, so in the window the ask is
  normally refused and the IndexedDB store above is what a page gets. Asking
  costs one refused call at startup and is what makes moving the session into
  a worker a change of where this runs rather than a change to what it does —
  the backend decides the `synchronous` pragma and how a wipe deletes, and
  nothing above `session/store/` learns which one answered.
- **Every tab is served its own copy of every frame.** The tab holding the
  account writes a history load once per connection, and the browser
  structured-clones each of them: two tabs is two copies of the same hundred
  chats, and the frames go to whoever asked rather than being shared. That is
  the right trade at the number of tabs a person opens and the wrong one at
  ten, and the shape that fixes it is the same one everything else here is
  waiting on — a `SharedWorker` holding the session, handing every tab a
  `MessagePort`, with one copy of a frame going out to a fan-out the browser
  does rather than one this side writes. It is also what would let a tab's
  media come from a `MessagePort` transfer rather than a clone. The obstacle
  is not the transport: it is that the session, the store and the bridge all
  change address space at once, which is the item below.
- **A tab that takes over restarts the session it inherits.** A follower
  promoted when the leader closes does not receive the leader's session — it
  starts one of its own: dials, hydrates from the store, and draws the account
  again a second or two later, with the window showing its reconnect while it
  does. Nothing is lost, because everything was committed to the store by the
  tab that had it, and nothing is corrupted, because the lock is what serialises
  the two. But it is a reconnection where a handover would be seamless, and a
  handover is not something two agents can do with a session in one of them.
  The `SharedWorker` above is the answer to this one too: there the session
  outlives every tab, so the last tab closing is the only thing that ends it.
- **A page's plugins share its one agent, and a worker is what would end
  that.** They run now — a task each on the page's loop, their modules in
  OPFS, their approvals in `localStorage` — and what is left is isolation
  rather than capability. A desktop plugin owns a thread, so a handler that
  spends its whole fuel budget costs a core nobody was using; here it costs
  the frame the page was about to draw. Fuel bounds one call and `MAX_DUTY`
  bounds the sum of them, so the ceiling is a known one, and it is still a
  plugin the user can feel. Loading is the same fact at its worst: `start` is
  `async` and yields to the page between modules (`sched::breathe`), so
  `MAX_LOAD_TIME` bounds the loading rather than the length of a freeze — but
  one module's own `oxi_init` is a synchronous call with a fuel budget and
  nothing to yield at. The answer is the one the store is already
  waiting on: a dedicated worker per plugin, its queue a `postMessage` port
  instead of a channel on this loop. That is a second scheduler rather than a
  second backend, which is why it is not done here.
  Two smaller things go with it. A page reads every installed module before
  it starts any of them — `Plugins::start` takes a closure per module and the
  desktop opens them one at a time, but nothing in a browser can open a file
  lazily from a synchronous loader, so `MAX_TOTAL_BYTES` bounds the folder
  where the desktop bounds the file, and installing checks what the folder
  *would become* rather than what the new module weighs — under a Web Lock,
  since the folder is the origin's and two tabs of it would otherwise each
  weigh a folder the other is about to grow. `MAX_PLUGINS` is asked at the
  listing rather than at the workers, for the reason the desktop's discovery
  truncates before it opens anything. A second plugin that fits alone and not beside the first would
  otherwise be written, reported as installed, and skipped at every load
  after.
- **A page with its own session uploads unverified against the CDN's CORS.**
  The blocker that used to be here is gone. `BrowserHttpClient` implements
  `execute` and nothing else, which the trait allows — the streaming paths
  default to refusing — and the library's buffered `Client::upload` now sends
  the body through `execute` rather than reaching for `execute_upload`, which
  only `upload_stream` still uses. So the staging, the container and the
  upload are all answerable from a page, and `platform::capabilities` no
  longer withholds the microphone from one.
  What is not established is the preflight. A page's origin is not
  `web.whatsapp.com`, and an upload is a `POST` carrying
  `Content-Type: application/octet-stream`, so the browser asks the CDN's
  `OPTIONS` first — a question a download, which is a plain `GET`, never
  raises. If some host refuses it the send fails where every other send
  failure lands: the bubble goes Failed, with a retry that re-sends the bytes
  already encoded rather than re-recording them. Worth measuring against a
  real account before this line is deleted.

- **An attachment is sent as it is, and without a caption.** Picking a file
  sends it: there is no step between the chooser and the send for a caption to
  be typed in, for a photo to be cropped, or for the kind to be overridden —
  and the composer's own text stays a message of its own rather than becoming
  one, because taking it at the press would lose it to a dismissed chooser and
  taking it at the completion means reaching for a `Window` from inside an
  async continuation. The protocol carries `caption` and `kind` per file
  precisely so that step is a front-end change rather than a protocol one.
  Two smaller gaps go with it. A video is sent without a poster frame: the
  `jpegThumbnail` is what the recipient draws before downloading anything, and
  producing one means decoding H.264 inside the process holding the account —
  the decoder is in `oxidezap-video`, which the session already depends on, so
  what is missing is a first-frame path rather than a decoder. And a picture
  is uploaded at its own size: a photo whose format the recipient will not
  draw is re-encoded now, but nothing is *scaled down*, so a 12 MP photo goes
  over a phone connection at 12 MP, where WhatsApp's own clients would have
  resized it first.
  The preflight question above is this feature's too, and more so: a photo
  from a page takes exactly the route a voice note takes, and there are more
  of them.

- **An animated picture arrives as its first frame.** A photo message carries
  a photo, so an animated GIF or WebP is decoded, flattened and re-encoded as
  a still JPEG like any other picture whose format the other side will not
  draw. It is an improvement on what it replaced — the animation was not
  playing before either, it simply rendered as nothing — but it is not what
  the sender meant. WhatsApp's own clients send motion as a *video* with
  `gifPlayback` set, which is the shape this wants; what stands in the way is
  an encoder, since nothing in this tree writes H.264. Sending it as a
  document instead would keep the file intact and lose the inline preview,
  which is the other half of the trade and is why neither is done yet.

- **A video with B-frames is ordered correctly and has never been played.**
  The indexing model was the fix and it is done: `demux::Timeline` reads the
  composition offsets the container carries (`Mp4Sample::rendering_offset`),
  every index above the demux is a rank in presentation order, and the only
  thing that still counts in decode indices is the loop that hands units to a
  decoder — so a seek is the samples a presentation position depends on
  rather than a range, and the stamp a unit is fed under is where its picture
  belongs rather than where it was fed. What is *not* done is playing one:
  the B-frame fixture the tests build is a container, not a stream, because
  no real capture may be checked in and nothing in this tree encodes H.264 —
  so the ordering is verified at the demux, which is pure logic, and the
  WebCodecs half is verified by reading. The desktop's decoder is the other
  half of the same gap: openh264 does its own reordering, and whether the
  picture it hands back for a fed sample is the one this labels is untested
  against anything but a baseline stream. Both want a real B-frame video and
  a browser to play it in.
- **A follower tab cannot place a call, and the reason is which document owns
  the devices.** A tab that lost the claim holds no session, so its Place or
  Accept is executed by the tab that does — and `getUserMedia` and
  `AudioContext::resume` then run in *that* document. The microphone, the
  speakers and the permission prompt would all be the leader's, in a tab the
  person pressing the button is not looking at and has not gestured in, so
  the call would be held by a tab that did not ask for it and heard there
  too. `calls_unavailable` refuses it and says which tab to use. It is the
  one place a follower differs from a desktop window talking to an
  `oxidezapd`, and the contrast is what makes it right: there the devices are
  the daemon's by design and nobody expects the window to hold them, while
  here both tabs are windows and the wrong one would. Fixing it properly
  means the follower opening the devices and handing them across, which is a
  change to the tab protocol rather than a check.
  It is a *separate* question from `calls_unavailable`, and folding the two
  together was a bug rather than a tidy-up: a window that cannot carry a call
  owes the caller an answer and declines, while a window that is merely the
  wrong one owes them nothing — the call is answerable in the tab beside it,
  and declining would send `Decline` to the leader and clear the offer
  everywhere, telling somebody to answer in the other tab while destroying
  the call they would have answered there.

- **A call's devices are held open by the engine, so letting go of them is
  evidence.** The two channel ends handed to the library — the receiver it
  takes microphone frames from, the sender it plays the peer out of — are the
  whole of what keeps a call's audio graph alive. An engine that runs a
  conversation and stops releases both when its driver returns, and one whose
  driver returns without ever using its transport releases them at the same
  instant in the same way. From inside the graph the two are identical, which
  is why a browser call that ended a moment after connecting produced three
  reports with nothing in them. `audio::call_ending` names which half went and
  the relay says whether it ever carried a packet; together they separate a
  call that ended from one that never started, which no single line on either
  side can. Portable and tested off the browser, because the rule is about
  channel ends rather than about devices.
  Two things keep that evidence honest, and both are the same mistake in
  opposite directions: attributing to the far side something this side did.
  A microphone unplugged or revoked is closed *here*, from the track's own
  `ended` handler, and the sender closing is the same observation either way
  — so the capture arm asks whether that happened and reports `CaptureLost`
  rather than blaming the engine for a device that went away. And the relay's
  first-packet marker is set after the browser has *accepted* a send, never
  before: a rejected send and a packet dropped for congestion both return
  early, and either would otherwise let a channel that carried nothing be
  released claiming it had — nor from a send that *returned*, since a channel
  that is `closing` or `closed` has the agent buffer the data rather than
  throw, so `Ok` there is a packet that will never leave. `CallAudioFacts`
  carries the third: endpoints dropped before any engine received them — a
  call hung up while `getUserMedia` was still in front of a permission prompt
  — release both ends at once in exactly the way a driver returning does, and
  that ordinary cancellation is not evidence about a driver there was none
  of. Which is why the handoff is marked after the `start()` that took the
  endpoints, and *before* the `start()` that takes them rather than after:
  `start()` awaits and what it spawns is the driver, so on a page — one loop
  for every task — a driver that takes the endpoints and returns while
  `start()` is still pending drops them before a later mark could run, and
  the ending would read as never handed over for exactly the call the flag
  exists to explain. Where it sits is the real transfer: the builder holds
  the endpoints, nothing above may return any more, and no `await` separates
  it from the handover. Every exit before that drops the builder with them
  inside it. A local loss outranks that gate rather than
  being filtered by it — a microphone unplugged while the *camera* is still
  opening has not been handed over and has not been cancelled either, and the
  device is the only evidence there is. It outranks the *arm* as well: a local
  loss and an engine letting go leave both futures ready before the race is
  polled, so what this side knows is read over which future won rather than
  inside it. Safe to prefer because the teardown's own `stop()` does not fire
  `ended` — nothing on the way out can set that flag.

- **An abort drops a future where it stands, and an unpolled future leaves
  nothing behind.** The library ends work by aborting the handle its runtime
  handed back, and `set_media_task` uses that deliberately: a media task whose
  call is already gone is aborted the moment it is handed over, and the driver
  task is written so that even an abort before its first poll releases the
  call — `_ended_guard` drops and notifies, the audio feeds drop and close the
  device. Correct, and completely silent: no error, no event, no log, because
  nothing failed. From a console it reads as a call that was offered and then
  ended a moment later with nothing in between, which is exactly how it read.
  So `BrowserRuntime::spawn` says which of the two endings happened. One line
  per aborted task, against a class of failure that otherwise leaves a report
  with no evidence in it.

- **Which end a full queue drops from is a question about the payload, not
  about latency.** The microphone's queue evicts its oldest frame and the
  camera's refuses its newest, and the two look like the same decision made
  inconsistently. They are not: a PCM frame stands on its own, so dropping an
  older one costs exactly that frame and the newest speech is the only speech
  worth having. An H.264 picture is referenced by the ones behind it, so
  evicting the oldest does not free a slot — it makes everything still queued
  undecodable and then sends it, and the peer receives two corrupt pictures
  where refusing the new one sends two good ones and a gap. The camera is
  staler by two frames, 66 ms at 30 fps, and that is the whole price of
  keeping what is delivered decodable. Both ask for a keyframe on the drop,
  because the gap is real either way.

- **A dropped access unit is a frame of RTP time that goes unspent.** The
  library's `VideoSource` advertises one `rtp_timestamp_stride` and advances
  by exactly that per unit delivered, and `EncodedFrame` carries no timestamp
  — so the stream's clock counts *units*, not elapsed time. Everything on this
  path drops on purpose (the encoder's own queue, the plane's, and the web
  timer's backpressure skip), and each drop is therefore a frame's worth of
  time the video clock never advances through: under sustained loss the
  picture's timestamps fall behind the audio's, by the length of what was
  dropped. Predates the browser backend and is identical on the desktop —
  `camera.rs`'s `try_send` and `plane.rs`'s both drop into the same
  fixed-stride source. Closing it means a timestamp on `EncodedFrame` and a
  `VideoSource` that reads one, which is a change to `whatsapp-rust` rather
  than to anything here; the alternative — not dropping — is the one thing
  this path exists to do.

- **Group video is drawn but not reachable.** `call_card/video.rs` carries a
  participant grid the library's group calls would fill; 1:1 is what the card
  routes to today.
- **Only some failures reach the person who asked.** `app/notices.rs` is the
  transient surface the app had been missing: one sentence, expiring on its
  own, changing no state, drawn by the root over whatever screen is up — the
  other end of the scale from `AppState::Error`, which leaves the connected
  view and schedules a reconnect and is catastrophic for a save that did not
  start. A failed save and a failed recording go through it.
  What still does not is most of what the *daemon* refused: a front end learns
  only `Accepted`, and a refusal reaching the window would need a field on the
  wire. `SendFailed` is the one exception, and it is against a chat rather
  than against the request. A download is the second, and it is the shape the
  rest of them would take: `ProtocolError::Failed` says the daemon tried and
  something outside the request went wrong, and carries whether asking again
  could work — because that is the only question the person actually has, and
  a full disk and a dropped connection are the same sentence without it. The
  answer decides what the notice ends in; `app/notices.rs::what_went_wrong`
  is where the bit is spent. `CallMediaFailed` is the third, and it was added
  after a browser call that dialled no relay read in the console as an offer,
  an ending, and not one line between them: the library publishes
  `MediaSetupFailed` with the reason and the event pump's catch-all was
  throwing it away, so the one event carrying the explanation was the one
  nothing listened to. A call that ends a moment after it is placed has to
  say why, or every report of it is a bug report with no evidence in it.
- **A promised file is not a held file, once the reader is a browser.** The
  daemon's media cache is files and no index — the front end it was written
  for opens them itself, so `claim` can be `has` and there is no window
  between promising a key and handing it over. A page attached to that daemon
  reads over HTTP instead, which makes the promise and the read two round
  trips, and a `ClearMediaCache` landing between them deletes a file already
  reported as downloaded. Not the budget sweep, which drops the oldest and so
  never the key just written; and the cost is one refetch, since media the
  renderer does not have is drawn as an offer to download. Closing it means
  the native cache keeping claims the way the page's does, which is the index
  that module opens by saying it does not have — worth it only if somebody
  meets it.
- **What the module weighs is nobody's job to notice.** The Pages workflow
  prints it now, and the numbers to compare against are: 29,825,238 bytes at
  `17e6d4f`, of which the code section is 84.5% and the data section 15.1%,
  with no name section at all (`strip = true` removes it before wasm-opt
  sees the module — which is also why a DevTools flame graph of this page has
  never had Rust symbols in it). By group, that code is 29% gpui and its
  renderer, 21% the Rust standard library, 17% the WhatsApp protocol and
  crypto, and 5% gpui-component. `wasmi`, `symphonia`, `mp4`, `opus`,
  `openh264`, `tree_sitter`, `notify` and `tracing` are all absent: LTO
  removes them, and the gates that exist for them are discipline rather than
  bytes.
- **The page's media budgets are one number and three ceilings.**
  `WEB_MEDIA_BUDGET_BYTES` is what the daemon's cache and a frame's fetch each
  allow, and `DECODED_IMAGE_BUDGET_BYTES` is a quarter of it again on top — so
  the worst case a page holds is their sum rather than the figure any of them
  names. Naming them in one place makes them move together and makes the
  arithmetic possible; nobody has done the arithmetic. Coordinating one
  allowance across three caches in two crates wants a measurement of what a
  page actually holds, which is the same measurement the item below needs.
- **What evicts the media a conversation is holding is coarser than a
  viewport.** There is a policy now — `Chat::release_media` is the
  arithmetic, `RETAINED_MEDIA_BUDGET_BYTES` the allowance, and
  `WhatsAppApp::sweep_retained_media` the judgement — and a released row is
  left in the state a row whose media never arrived is already in, so it
  draws the offer to download the renderer has always drawn and the press
  that accepts it fetches. What the front end can actually answer is *which
  conversation is on screen* and *what it is holding open*; inside the
  visible conversation the newest rows keep their bytes and the rest let go,
  which is not the same as "far off screen". gpui's `list` hands no visible
  range out of its render pass, so a reader scrolled up into an album can
  have a picture released out from under them and drawn as a download. The
  honest fix is a touch signal from the row render — the decoded-image cache
  is already an LRU keyed by message id and could carry it — and the reason
  it is not here is that it wants a window to measure in, which is exactly
  what this environment has none of.
- **A withdrawal is applied before it is written down, and that is a trade.**
  A revocation clears the shared mask first and persists second, so the very
  next command a draining backlog attempts is already refused. The cost is a
  crash in the window between the two: the file still holds the old grant,
  and the next start reads it. Reversing the order buys durability and sells
  the live account — the plugin would keep its permissions across a disk
  write while Settings had already redrawn — and there is no ordering that
  closes both, because closing the crash window means the write happening
  first. Protecting the account that is running now is the side worth taking;
  the failed-write path already removes the file rather than leave a stale
  grant, so only an actual crash, in that window, reverts anything. Which is
  also why the *rename* is made durable: syncing the temporary file persists
  its contents and not the directory entry that names it, so without a sync
  of the parent a revocation could be undone by losing power at any point
  after it looked finished — a far wider window than the one above, and the
  half of this that is fixable rather than a trade.
- **A withdrawal does not reach a command already in flight.** The mask is
  read live, so the *next* command a plugin attempts is checked against the
  answer — but the check and the send are two steps, and the send parks on a
  bounded channel. A revocation landing in between does not stop the command
  that is already waiting there, so one send, read receipt or typing update
  can still act after Settings says "not allowed". The window is bounded by
  the session's own draining, and closing it means carrying the plugin's
  authorization into `SessionCommand` so the executing side can check it
  again — a change to the command shape, decided on its own.
- **A plugin cannot tell "cleared" from "not carried".** The ABI's absence
  rule is that a field's absence reads back as its default, and a string's
  default is empty — which is exactly what makes adding a field a non-event,
  and exactly why a reaction that was *removed* (an empty emoji) and a text
  field committed empty arrive indistinguishable from a field the event never
  had. Smuggling the difference into string presence would break the rule for
  every reader; carrying it needs a field that says so.
- **A plugin never sees what this process sends, at the time it is sent.**
  `kinds::MESSAGE` is what *arrives*, including a message this account wrote
  on another device — but a send made through this daemon is announced as an
  id assignment, not as a message, so nothing reaches a plugin at send time
  and one keeping a record of a conversation has a hole in it exactly where
  its own replies go. Whether the same message comes back later is the
  server's business rather than a promise: when it does, through a history
  sync, it arrives as an ordinary `MESSAGE` with `FROM_ME` set. Which is why
  synthesizing one at send time is not free — a plugin would see the ones
  that do come back twice.
- **A plugin's reply quotes an empty message.** `oxi_send_reply` names an id
  and nothing else, which is all the ABI gives a plugin — but the session
  does not re-read the original: `quote_context` puts the preview, the sender
  and the kind straight on the wire, so the peer sees the reply's linkage
  over a blank quote bar, and in a group it names no author. Resolving it
  needs a lookup by id, which the daemon has no store to make and the session
  has no method for; the alternative is widening the ABI, which is a decision
  of its own.
- **A front end cannot say what went wrong with a command.** `Accepted` means
  the session took it; per-request outcomes would need request ids on more
  than downloads. A failed send arrives as `SendFailed` against the chat, not
  against the request that caused it. A *plugin* does learn this, which is the
  odd part: its call is synchronous, so there is nothing to correlate.
- **A plugin cannot reach the network or the disk, and half the interesting
  ones want to.** A translator, a webhook bridge, a conversation export. Each
  is one import, and each turns the categorical sentence in docs/gotchas.md
  into a policy — so it wants a declared destination, a prompt at enable time,
  and a decision of its own.
- **A plugin's tree is state, and state frames are the ring's to hold.** Every
  change to any plugin publishes the *whole* set — that is what makes a
  mid-stream join safe, and it is also what a stalled client's 256-frame
  backlog then holds a copy of. The arithmetic is `MAX_PLUGINS` trees of
  `ui::MAX_BYTES`, decoded, times `BROADCAST_CAPACITY`: hundreds of megabytes
  in the worst case, transiently, before that client is cut to a `Resync`.
  Bounded and recoverable, but larger than it should be, and the plugin half
  is the half somebody else writes. Coalescing pending frames or publishing
  per-plugin deltas would fix it and is a change to the state protocol —
  every frame there carries a version and a client's whole recovery story is
  built on their being contiguous — so it is a decision of its own rather
  than something to bolt onto the plugin path.
- **"Only this user can write it" is a POSIX sentence.** `only_this_user_can_write`
  reads an owner and a mode, which Windows does not have — it answers `true`
  there, and what stands in for it is where the directory *is*: plugins and
  their state live under `%LOCALAPPDATA%`, whose ACL is the profile's. That
  covers the default and not an override, so a `OXIDEZAP_PLUGIN_DIR` pointing
  at a share is trusted on Windows and checked on unix. It is the user's own
  environment variable naming their own directory, which is the weakest half
  of the threat this guards against — but it is a gap, and closing it means
  reading an ACL and deciding what "only this user" means when the answer is
  a list rather than three bits.
- **A reload costs a page a plugin's last settings write, and that is the
  stamp working rather than failing.** Taking a fresh storage handle retires
  every older one, so a plugin whose key-value state is still dirty inside its
  one-second write throttle loses it: the retired task reaches its final
  `flush_settings` after the swap, and the new handle's stamp refuses it. A
  desktop does not have this — `retire` joins the thread, so the write has
  already happened. It is the same trade the stamp exists for, taken in the
  same direction: the alternative to refusing that write is letting a departed
  host's in-memory settings land on top of what the new one has already
  written, which is worse and is what the stamp was added to stop. What is new
  is only that a reload makes it reachable mid-session, where before it took a
  tab reload. Ordering it properly means retiring and flushing the old tasks
  *before* the handle is taken, which a page cannot do by waiting — it cannot
  join a task on its own loop — so it is the worker-per-plugin item below
  rather than something to bolt on here.
- **A reload is the folder's, not one plugin's.** Reloading one of five
  restarts five, because an id is what an approval and a settings document are
  keyed on: two generations holding one id would be two plugins sharing an
  identity, which is the thing the host refuses everywhere else. Reloading
  just the module that changed means keeping the rest alive across the swap,
  which is that same sentence with an exception carved into it, so it is a
  decision of its own rather than an optimization.
- **There is no message interception.** A plugin that could alter or block an
  inbound message would sit between the store and every front end, which the
  whole state model assumes it cannot — plugins observe and act, they do not
  filter.

Clickable `div`s that remain are deliberate: a chat row and a media thumbnail
are surfaces, not commands, and have no semantic component to compose from.
Anything that *is* a command (call accept/decline, back) is a `Button`.
