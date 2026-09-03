# Gotchas

Non-obvious behaviour, and the reasoning behind it. Read the entry before changing the code it describes.

- **A *transport's* platform split lives in exactly two places.**
  `ipc/endpoint/` is the client end and `daemon/listener/` is the server end;
  everything above them — framing, requests, the whole protocol — is written
  once. Transports only: a capability crate owns its own split, which is what
  `audio/src/web/`, `video/src/web/` and `session/src/exec/` are, and a new
  backend for one of those does not belong under ipc or daemon. A Unix socket is
  a filesystem entry that survives a crash and a named pipe is a name that
  does not, which is why reclaiming a stale endpoint exists on one and not the
  other — and why the Windows listener builds a security descriptor by hand,
  since a named pipe's default grants read access to `Everyone` while a Unix
  socket inherits a `0700` directory. A client checks who answered on both,
  and for one reason: the name is predictable and not reserved, so somebody
  else can be there first — the socket under the `/tmp` fallback, the pipe at
  `\\.\pipe\oxidezap-<SID>`. `first_pipe_instance` on the listener guards the
  daemon once it exists, which is the wrong half of it: the daemon then
  refuses to start and the client talks to whoever got there. The kernel knows
  who is on the other end either way — a peer uid on the socket, the serving
  process's token SID on the pipe.
  Two more transports joined them rather than becoming new places:
  `endpoint/web/` and `listener/web/` are a WebSocket, because a page can
  open neither of the others, and `endpoint/tab.rs` and `listener/tab.rs` are
  a `BroadcastChannel` between two tabs of one origin — the tab holding the
  account serving the tabs that do not, which is the same daemon-and-front-end
  split a socket carries, in a browser that has no socket to carry it. What every transport shares on the way out is
  `ipc::Link`, one `Send + Sync` handle with the platform's own object behind
  it — load-bearing on the web, where a `web_sys::WebSocket` is neither and so
  cannot be held beside a front end's state at all; it holds a queue into the
  task that owns one instead. What they do not share is the way *in*: a
  process parks a thread in a read and a page is handed a callback, so the
  read halves stay apart and what they meet at is `session/frames.rs`, which
  is the whole protocol state machine and is written once.
  The server side repeats none of it either — `serve_client` was already
  generic over `AsyncRead + AsyncWrite`, so the bridge hands it one end of a
  `tokio::io::duplex` and moves the lines across as text frames.
- **A loopback port is not a Unix socket, and the difference is the whole of
  the web bridge's design.** A socket has file permissions and a peer uid to
  check; a TCP port has neither, and a WebSocket is not subject to the
  same-origin policy — so any page in the user's browser can open one to
  `ws://127.0.0.1` and would otherwise be handed the message history and the
  ability to send. Hence: off unless asked for (`--web`), loopback unless told
  otherwise, and every browser origin refused unless named (`--web-allow`),
  excepting localhost, which is the developer's own `trunk serve`. But an
  origin is not the admission check and could not be: a loopback port is
  reachable by *every account on the machine*, while the socket sits in a
  `0700` directory and answers a peer uid — so reaching the socket proves
  being this user and reaching the port proves nothing, and any local account
  can write `Origin: http://localhost`, which is a string. A token in that
  same per-user directory is what carries the guarantee across: drawn once and
  kept, so a bookmarked URL survives a restart; required on the upgrade and on
  media alike, since a photo is as much the account's as a frame is; compared
  without an early return, so the matching prefix is not something a caller
  can time; and answered with a `404` rather than a `403`, because an endpoint
  the caller may not open has no reason to confirm it is there. A request with
  no `Origin` carries nothing to check — an `<img>`, a `<script>` and a form
  GET are browser requests that send none — so it is served on a loopback
  bind, and still only with the token, which is the whole admission check. A non-loopback
  bind is an error rather than a warning: there the header is a string the
  client picks and the traffic is cleartext, so remote access is a tunnel's
  job. Both endpoints draw on one admission
  count, because a client costs the same descriptors and tasks however it
  arrived; the web one claims its slot at the upgrade rather than at accept,
  since the same port serves media and a photo is not a front end.
- **A browser API never gets a view into wasm memory.** This module is built
  with `--shared-memory`, so a `Uint8Array` over the linear memory is a
  *shared* view — and the specifications refuse those: "The provided
  ArrayBufferView value must not be shared." `WebSocket.send` refuses one and
  so does `RTCDataChannel.send`, so anything crossing out is copied first
  (`js_sys::Uint8Array::from(&data[..])`, then the buffer), never
  `send_with_u8_array`. The socket learned this and wrote the rule down; the
  relay was written afterwards and passed a view anyway, which is worth more
  than the rule itself: **every** relay send threw, from the first browser
  call ever placed, and the drive loop answers a failed send with
  `break 'drive` while publishing nothing — so a call opened its relay,
  released it, and ended with no error anywhere. Four production logs came
  back empty because of it. What a transport may *not* do is stay quiet about
  a refused write: the driver discards the reason, so this side logs it.
  WebCodecs is the exception and by declaration rather than by luck — its
  buffers are `AllowSharedBufferSource` in the IDL — which is why the video
  path never hit this.
  Web Audio is the third crossing to learn it, and the one that hid longest:
  `AudioBuffer.copyToChannel` takes a `Float32Array` that "must not be
  shared", so `copy_to_channel`, which takes `&[f32]`, threw on every block a
  call ever played — the peer's audio arrived, decoded to PCM and was written
  nowhere. `copy_to_channel_with_f32_array` and a `js_sys::Float32Array` are
  what it takes. The *read* direction is safe and that asymmetry is why only
  one direction of a call was broken: `get_channel_data` copies JS-to-wasm and
  the microphone worked throughout. Verified in a real cross-origin-isolated
  page rather than reasoned about — `copyToChannel` and `copyFromChannel` both
  refuse a shared view, and a `ScriptProcessorNode` with zero input channels
  fires perfectly well, which is what the evidence first pointed at and is not
  what was wrong.
  What a transport may *not* do is stay quiet about either direction, which
  is why the relay accounts for three things and not two — outbound, inbound,
  and inbound *media* separately. The relay answers our STUN allocate whether
  or not it ever bridges the peer to us, and an unanswered allocate ends the
  call in ten seconds, so a call that lived longer certainly received control
  traffic: a single "did anything arrive" flag reports the broken call as
  healthy, which is the opposite of what it is for. What separates them: a channel that opens, carries our media
  and is released having received nothing is a different fault from a peer
  who said nothing, and the two read identically in a log that reports
  neither. The first call of a production session was exactly that — not one
  decoded frame in twenty-one seconds, while the second call carried
  thousands — and nothing anywhere could say whether the relay never spoke,
  the peer never reached it, or the drive loop never listened.
  Three outages was enough, so the spelling is now banned rather than
  remembered: `clippy.toml` lists the `&[u8]`/`&[f32]` bindings under
  `disallowed-methods` with the copying replacement in the reason, and CI's
  `Test (web)` job runs that one rule against the wasm target — the only place
  it *can* be run, since these crates' browser bindings compile for no other
  target and the workspace `Check` job never sees a line of them. Only that
  rule: neither crate has ever been clippy'd for wasm and both carry lints of
  their own, and adopting that surface is a separate cleanup.
- **An abort is something said, not something let go of.** The library's
  `AbortHandle` tells its two endings apart by whether it *calls* the closure
  it boxes — `abort()`, and `Drop`, call it; `detach()` drops it uncalled —
  so a runtime that cancels by dropping the sender makes `.detach()` mean the
  opposite of what it says. The web runtime did, and the tokio one does not
  (dropping a `JoinHandle` detaches), so it went unseen on a desktop and was
  total in a page: `runtime.spawn(…).detach()` is how the library runs nearly
  everything fire-and-forget — the QR rotation, inbound message handling, the
  bot's own subscriptions — and every one of them was cancelled before its
  first poll. What a page did instead was handshake, ack the server's
  `<pair-device>` from the handler that runs inline, and then sit there with
  no code on screen. `net::abort_requested` is the rule stated once: a value
  sent is an abort, a sender dropped is a detachment and waits forever.
- **A poisoned lock is answered by what the lock protects.** Two answers, and
  the choice is not a matter of taste: a lock over state whose invariants span
  several fields — the daemon's `Inner`, a call registry — is *panicked* on,
  because a holder that died mid-mutation may have left it torn and continuing
  publishes that. A lock over one collection whose every operation is atomic
  in itself — a memo, an ordering token, a map of lanes — is *recovered*
  (`unwrap_or_else(PoisonError::into_inner)`), because a `HashMap` cannot be
  left half-inserted and turning a naming question into a second panic is
  worse than answering it. Nothing under the release profile is reachable
  either way: `panic = "abort"` means no lock is ever poisoned there, so this
  rule is about tests and debug builds, which is exactly where a second panic
  hides the first.

- **A directory that was open is one whose contents are suspect.** Tightening
  the mode closes the door behind whatever is already inside, so the question
  after a `chmod` is what that is. Authority is deleted — the plugin host
  removes an `approvals.json` it finds in a directory another account could
  have written, because a `chmod` now does not make that file the user's
  answer. A cache is cleared for the same reason and at no cost: the daemon
  drops the media directory when it has to tighten its state directory, since
  a file planted under a content key would be served to the window as this
  account's own photo, and everything in there can be fetched again. And a
  directory that cannot be made private at all is refused: `usable_state_dir`
  runs without one rather than trusting it.

- **How loud the client is, is a setting rather than a launch argument.** The
  two processes each apply the level to themselves and each write it down —
  the window because it draws part of the log and a page with its own session
  draws all of it, the daemon because it holds the session and because a page
  keeps its choice in a browser store no daemon can read, so the next
  `oxidezapd` would otherwise start back at `info`. `ClientRequest::SetLogLevel`
  is the sentence between them, and it is applied *and* persisted where it
  lands: a person raising the level is asking about the session that is
  running, and the file is what keeps them from asking again after every
  restart. A failed write is a notice, never a refusal — the level did change,
  and what failed is only the memory of it. Which is also why the daemon is
  the interesting half: restarting it to raise the level ends the very
  connection being investigated.
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
  default. The way back is the same door: the tray's Hide, and a click on
  the icon while a window is attached, relay `HideWindow`, and the front end
  answers it by leaving — on a desktop the window *is* the process, and
  ending it is exactly what closing the window does, so the two cannot drift.
  There is no cheaper hide to reach for on Wayland, where a toplevel cannot
  be withdrawn and brought back by its owner, and minimising is not hiding
  (the issue was reported on exactly that). The icon click toggles on
  `has_window` for the same reason Open launches on it: a subscriber that is
  not a window must not turn a click into a hide nothing acts on. It also
  remembers the click before it (`window::Toggle`): a double click reaches
  the tray as two activations, and the first one's hide lands in full — the
  front end quits, the guard drops — before the second, which would then
  find no window and start one over the one just put away. The menu's
  first item is named from the same fact — Open or Hide, each doing only
  what it says — and is re-read when the host opens the menu, which ksni
  does only for a tray that implements `menu_about_to_show`; the tray
  otherwise hears only `TrayState`, which a window attaching does not move.
- **A chat's `participants` is not a roster, and the header's line comes from
  the daemon.** That map fills as senders are *seen* — one entry per person a
  live message has supplied a name for — so it answers "who has spoken here
  lately" and nothing else. A header that counted it told a fifty-person group
  it had one member, so for a long time the line under a group's name was
  simply left empty, which is what the issue reported. The membership list
  lives on the connection: the library keeps one per group because the *send*
  path needs it, patches it as add/remove notifications arrive, and
  invalidates it when the server says its snapshot is stale. So the front end
  asks — `ClientRequest::GroupMembers`, answered with a `GroupRoster` — and
  the session reads it through `Groups::query_info`, the cached, send-oriented
  view: a group opened twice costs nothing the second time, and a miss sends
  the participant hash so an unchanged group is answered `not-modified` rather
  than downloaded. The fuller `get_metadata` (subject, description, admin
  roles) has no cache in front of it at all and none of what it adds is drawn
  anywhere, which is why it is not what this uses. The ask is made every time
  a conversation is opened rather than once, because that is what keeps the
  line current after somebody joins, and the answer already held stays on
  screen until the new one lands. Names come from the same `NameBook` a bubble
  is named by, so nobody is "Ana" over their message and a number in the line
  above it; a member nobody has ever named is returned nameless and *counted*
  rather than spelled out, since a LID-addressed group would otherwise draw
  "Unknown contact" six times in a row. The count is the one thing that is
  always true, which is why an all-stranger group reads "50 members" — the
  answer `participants` could never give.
- **What a file is sent as is decided in the front end; what it looks like is
  worked out where the bytes land.** Two questions, and they are answered in
  two places because they have two different pieces of evidence. The *kind* —
  photo, video, document — follows the type the picker read, and it is a
  choice rather than a derivation: a picture sent as a document keeps its bytes
  exactly, which is a thing a front end is allowed to offer, so
  `OutgoingMedia::for_mime` is the default and the field on the wire is the
  answer. The *shape* — dimensions, duration, the small JPEG that rides on the
  message — is read from the bytes by `session/whatsapp/outgoing.rs`, which is
  the side holding them when the message is built. Deriving the shape in the
  window instead would mean decoding every picture twice (once to draw the
  bubble, once to describe it) and would put a thumbnail in a
  newline-delimited JSON frame, which is the thing `staged_key` exists to
  avoid.
  A picture is also *re-encoded* there, and that is the second thing the two
  places are split over. A photo message is expected to carry a photo format,
  and the expectation is the recipient's rather than this tree's: WhatsApp's
  own clients re-encode everything they send to JPEG, so a client written
  against what it actually receives has never had to decode anything else in
  that position. A WebP sent from here uploaded, delivered and was marked
  read, and drew as nothing at all on the recipient's Android — while the
  browser that sent it drew the same bytes perfectly, which is what makes this
  a rule about the far end rather than about a decoder. JPEG and PNG go out
  untouched (a screenshot is a PNG, and re-encoding one is the loss nobody
  wants); everything else this build can decode becomes a JPEG, at the same
  dimensions, out of the decode the thumbnail was already paying for. What
  cannot be decoded goes out as it came, because bytes nothing here can read
  are bytes nothing here can improve.
  Two smaller things fall out of having read the bytes at all. The message
  states the type the *payload* is rather than the one it was picked as —
  those are two different claims, and only one of them was read out of the
  file, so a JPEG that arrives named `.png` no longer goes out saying so. And
  transparency is *composited* rather than dropped: JPEG has no alpha channel,
  and `to_rgb8` keeps whatever colour sits under a transparent pixel, which is
  nobody's decision — an encoder may leave the last drawn colour or garbage,
  and a logo drawn on nothing then arrives on a field of whatever that was. It
  goes onto white, because a picture drawn with transparency is nearly always
  drawn for a light ground.
  The thumbnail is not a nicety: WhatsApp draws it while the file downloads,
  and this tree draws it too — the store keeps what was sent and hydration
  reads a sent message back through the same `media_of` an arriving one goes
  through, so a message sent without one is a grey box on *both* sides. A
  video has none, and that is the gap rather than a decision: producing one
  means decoding H.264 where the account is held.
  Nothing about the staging is new here. A picked file goes out exactly the
  way a voice note does — `Session::send_staged` is the one path, and the four
  media caches, the reservation order and the abandoned-upload race are all
  the ones docs/web.md describes.
- **A file's size is checked before its bytes are read, and the ceiling is the
  protocol's.** `oxidezap_ipc::MAX_STAGED_BYTES`, named there rather than at
  either end for the reason `STAGED_PREFIX` is: the daemon's write endpoint
  refuses anything larger, and a front end that learned the number from a
  `413` would have paid for the whole read — in a page, a copy of the file in
  a linear memory with a ceiling — to be told something `metadata` and
  `File.size` answer for nothing. Which is also why a chooser hands back two
  lists rather than a `Result`: picking four photos and one film sends the
  four and says what happened to the fifth.

- **An ending is claimed, not owned.** Two places want to publish
  `UiEvent::CallEnded` — the arm handling the peer's `<terminate>`, and the
  watcher parked on `wait_ended` that the resulting hangup resolves — and in a
  production log both did, twice per hangup. Which one is "the owner" cannot
  be read off the registry: media can end before the stanza arrives or after
  it, so in one order the watcher has already removed the entry and announced
  by the time the terminate is handled, and in the other the terminate has
  taken it before the watcher runs. Either way one of them looks like the
  owner and the other announces regardless. So `announce_ending` is a claim
  that is true exactly once per call id and the first caller wins. It lives
  inside `notify_call_ended` rather than at the call sites: there are four of
  them — the watcher and three exits in the accept path — and a peer hanging up
  mid-acceptance reaches two for one hangup, so a guard that has to be
  remembered at each is one that will be forgotten at the next. The record is
  bounded against *concurrent teardowns* rather than time, since both
  announcements of one ending come out of the same one. `CallEndedElsewhere` is not
  that duplicate and stays unconditional: it says *where* the call went, a
  second sentence rather than the same one.
- **A call is held by whoever holds the session.** `oxidezap-session` is what
  opens the mic, the speaker and the camera, so the process that owns the
  session owns the devices. That follows from the split rather than being
  chosen, and it is why a call still works with the window closed.
  On a desktop that process is the daemon. On a page holding its own session
  it is the page, which is the same sentence and not an exception to it — the
  devices are WebAudio and `getUserMedia` there, and the media reaches the
  relay through an `RTCPeerConnection` rather than a UDP socket (`session/
  relay/`). What used to be written here is that a browser had no audio codec;
  it was wrong about which thing was missing. MLow is pure Rust and is what
  WhatsApp's own clients negotiate. What a page has no such thing as is a
  socket.
- **A plugin is a front end that does not draw, and it runs in the daemon.**
  It sees the account's events and acts through the same command channel a
  window's requests go onto, so it has no privileged path to the session. It
  lives inside the daemon rather than behind the socket because the daemon is
  the only process holding the session, and wasm already supplies the
  isolation a process boundary would have been for — and the count of them is
  bounded (`MAX_PLUGINS`) because every other bound here is per plugin: a
  store, a queue and an OS thread are all spent before a module runs an
  instruction, so a folder somebody unpacked a bundle into costs a thousand
  threads before the socket opens. Counted at discovery rather than at the
  workers, because counting the workers counted the *successes*: a folder of
  modules that each fail — read, parsed, and given their initialization fuel
  to refuse in — never reached the cap at all. What wasm does *not*
  supply is a bound on time and on memory, which is why fuel metering and the
  resource limiter are not optional: a plugin that loops forever runs out and
  traps, and the daemon loses a plugin rather than a thread. Fuel prices *one
  call*, though, and a plugin needs nobody's permission to arm a timer — so
  one could wake itself at the floor, spend almost a whole budget in each
  callback and never trap, owning a core for something subscribed to no
  account event at all. The share (`MAX_DUTY`) is the bound on the sum:
  busy time against elapsed, over a rolling window, with the excess slept off
  before the next call — the *whole* excess, and asked for before the window
  may turn over. Both halves of that were bugs: a debt truncated at one
  window, or forgiven when the window rolled, lets a plugin gain time faster
  than it pays it and settle near half a core with `MAX_DUTY` reading a
  tenth. Throttled rather than stopped, because a plugin doing
  too much is not the same as one doing something wrong — and the sleep is
  taken in slices, since a plugin being held back is still one the daemon has
  to be able to join. The limiter
  bounds tables and instance counts and not only the linear memory's bytes,
  because a declared table is allocated at instantiation — before a
  fuel-metered instruction has run — so a byte cap alone is a bound on one
  allocation rather than on the plugin. Two allocations sit outside the
  limiter entirely and are bounded before they happen: the module's own bytes
  and whatever parsing them costs, which are spent before the store exists
  (`MAX_MODULE_BYTES`, asked of the file rather than of its contents), and the
  strings an event handle clones into the *host* (`MAX_HANDLES`) — a plugin
  asking for one list element until its fuel runs out would otherwise grow
  the daemon by far more than the sandbox advertises. What the *host* writes
  about a plugin — a refused tree, a dropped root — is charged to that
  plugin's own logging budget for the same reason `oxi_log` has one: it is
  the same journal, and an invalid tree is a line a plugin can ask for
  sixteen times a call without calling `oxi_log` at all. One allowance rather
  than one plus an unbounded second. Reading a field is
  bounded too (`MAX_FIELD_BYTES_PER_CALL`), which is the same sentence about
  the copy rather than about the allocation: `oxi_field_str` writes into the
  *plugin*, so nothing here grows, and a loop over one ordinary message with
  a large buffer still turns a callback's fuel into tens of gigabytes of
  memcpy. Per call and not per window, unlike the log and the commands: that
  cost is time inside the call, which is exactly what `MAX_DUTY` measures
  across calls and cannot measure within one. `oxi_log` is bounded
  for the same reason and refused while loading for the other one: writing a
  line is host I/O that fuel does not price, and a module the loader is about
  to turn away should leave nothing behind. What it writes is also escaped —
  a line break in a plugin's line is a second entry the host's `plugin x:`
  prefix never reaches, so a module nobody has approved for anything writes
  what reads as the daemon's own diagnostics. A `Store` is not
  shareable and a wasm call is synchronous and blocking, so each plugin gets
  an OS thread of its own rather than a runtime task, which would stall the
  accept loop for as long as it ran. wasmi and not wasmtime: no JIT, so
  nothing generates code inside the process that holds the account, and no
  component model, which is the trade the ABI is built around.
- **A plugin's whole outside world is the `oxidezap` import module.** There is
  no WASI — not a restricted one, none — so a `.wasm` a user downloaded cannot
  open a path or a socket because no function exists that would. It has
  storage, but not the *filesystem*: `oxi_kv_get`/`oxi_kv_set` are a map the
  host keeps in a file the plugin cannot name. That
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
  one whose queue is full. Declaring is a single act, once — and so are
  naming and subscribing — for the same reason: a plugin that declares the narrow mask it was approved for, sends,
  and *then* widens has already sent, and the wider surface reading as
  unapproved afterwards is no use to the message. Nor does any of it start at
  instantiation — a start section and `oxi_abi_version` are code the loader
  has not accepted yet, so every import refuses until the module is
  instantiated, its version answered and its exports found. A withdrawal
  clears the shared mask *before* it is written down, where a grant is
  written down first: both fail closed, and doing the write first left a
  plugin holding its old permissions across a disk write while Settings had
  already redrawn. And an id may be claimed by only one file — two claiming
  it are two plugins sharing an identity, so withdrawing would reach one and
  leave the other acting. Otherwise a
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
  otherwise write its settings over everybody's permissions. That directory
  is made private *before* it is read, and the answers already in it are
  asked about after that door is shut — a directory that was open is one
  somebody else may have left an `approvals.json` in, and a `chmod` now does
  not make that file the user's answer, so it is deleted rather than ignored.
  A directory that cannot be made private is refused outright
  (`usable_state_dir`): a file saying what a plugin may do to the account,
  read out of a directory another local user can write, is a mask somebody
  else chose — and tightening the mode afterwards puts it in memory first.
  Refusing means no state directory at all, which fails closed: plugins draw
  and keep settings in memory, and everything touching the account is
  unapproved until somebody says yes in this session. It is also
  `%LOCALAPPDATA%` on Windows and never `%APPDATA%`, the same side the store
  is on: a roaming profile carries a file to another machine, and everything
  here is scoped to the account this one is paired to. Retiring it is a
  delete plus a `sync_dir`, for the reason the revocation's rename is
  flushed — an unlink that has not reached the disk is an `approvals.json`
  that comes back after the credentials have already been wiped.
  What an answer is recorded *against* is the id and the mask, deliberately,
  and not a hash of the module: replacing `autoreply.wasm` with different
  code keeps the answer. That is defensible because the mask is the whole
  authority — there is no WASI, so what the new code can do is exactly the
  sentence the user agreed to, enforced whatever the bytes are — and because
  the alternative asks again on every update, which is the surest way to
  teach somebody to dismiss the question. It is a real trade rather than an
  oversight: binding to the bytes would say "you approved this build", which
  is stronger and costs a prompt per release. It is also why nothing loads
  out of a place another local account can write
  (`only_this_user_can_write`: owner *and* mode, the directory and every
  module in it — a POSIX sentence, and docs/roadmap.md carries what stands in
  for it on Windows) — and a symlink is refused rather than followed, since
  following one answers about the target and says nothing about who may put a
  different file there: a target this user owns, `0600`, in a directory
  somebody else may write is a file they can unlink and replace, and the
  replacement inherits the id's approval. Allowing the link would mean a
  verdict on its directory, and on that directory's directory, with a race at
  every step; `OXIDEZAP_PLUGIN_DIR` is how a module is loaded from somewhere
  else, and it is checked the same way. An answer recorded against a name
  rather than against bytes
  is one somebody else's file under that name inherits — and a writable
  directory is one where a new name can appear, not only new bytes under an
  old one.
- **An event is a handle, not a payload.** Nothing is serialized for a plugin:
  it reads fields through four host functions against a table of constants, so
  a handler that looks at the text and the chat pays for two strings out of an
  event carrying a dozen, and the whole path is cheaper than the JSON one a
  socket front end already uses. What a plugin is *handed* is decided before
  any of it is built, though — `event::kind_of` answers from the session
  event alone, so a plugin watching messages does not pay for an account's
  receipts and presence, which are most of its traffic. Two matches that
  disagree would be a plugin silently missing events, which is what
  `every_converted_event_is_one_the_filter_admits` exists to refuse. Field
  ids are constants rather than one
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
- **A reload retires before it loads, and that order is the design.** The
  obvious alternative — build the new set first, keep the gap short — creates
  the one thing the host refuses everywhere else: two live workers under one
  id, each with its own mask, for the length of the load, so withdrawing a
  permission reaches one of them and leaves the other acting. So the old
  generation is retired first and the gap is real: for as long as loading
  takes, nothing is observing the account. That is the honest cost of somebody
  deciding to change what is running, and it is bounded by `MAX_LOAD_TIME`
  like every other load; what is *not* lost is anything a plugin wrote down,
  since its settings and its approval are in storage and the next generation
  reads them back.
  Retiring is three things and each answers a platform. The registry is
  retired *first*, because a worker's last act is very often to publish and a
  page cannot wait for it — one late `set_roots` from a plugin nobody is
  running would draw the departed set over the live one. The queues are then
  dropped and the threads joined, which is what a desktop has. And the masks
  are zeroed *after* the join, which is the same sentence in the other
  direction: on a desktop the handler has already finished so it changes
  nothing, and on a page the task is still on the loop with a call left to
  make — one that may no longer touch the account. It is the same zero a
  withdrawal writes, so it is one mechanism aimed at a generation rather than
  a second one.
  What orders a reload against `shutdown` is two atomics rather than one, and
  they have to be in the same total order. A reload asks whether the host has
  been retired *after* it holds the reload slot, and `shutdown` raises that
  flag *before* it waits on the slot — which is the store-buffer shape, so
  relaxed on both sides lets each miss the other: the wait reads an idle slot
  while the claim that has just been made reads a flag that is not yet set,
  and the wipe then runs beside a generation being built. Both are `SeqCst`
  for that reason, and neither is on a path anything measures. Being
  `SeqCst` on the *slot* buys nothing here: an ordering on one atomic says
  nothing about another.
  A per-entry read failure is not a folder that cannot be read. The folder is
  answered with three outcomes — absent, refused, unreadable — because a
  reload that took "unreadable" for "empty" would retire every healthy
  plugin; a single module that will not open stops that plugin and no other,
  on both platforms, which is what a desktop *has* to do since its `open`
  runs after the old generation is already gone. What a reload answers with
  is therefore an outcome rather than a count (`Reloaded`): three of the four
  are zero plugins installed and mean entirely different things, and the
  count is what the daemon writes to its log — a deferred pass reporting
  "plugins reloaded: 0 running" over five healthy plugins still running is
  the line that made this an enum.
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
  reach. An action is checked against that tree before it is routed, rather
  than against the plugin merely being loaded: a front end's frame can be
  older than the daemon's, so a second window still showing a button since
  withdrawn or greyed out would land as a real press, and an id the plugin
  never published would reach a handler as a widget that does not exist —
  and the check is on the widget's kind as well as its name, since a plugin
  may republish a button as a text field under the same id and an older
  window's press would arrive as that field's commit carrying no value. An
  id names one widget *within a slot*, which is where the encoder refuses a
  duplicate: across slots it may repeat, because an action says which one it
  came from, but twice in one slot nothing tells the two apart — a press
  names both, and a front end keeping a text box per id draws one box for
  two fields. In the slot the action says it came from, because one plugin may draw the same
  id in a header and in its settings panel: withdrawing one of them must not
  leave the other vouching for it, which is why the slot travels on the
  action rather than being guessed from whether a chat came with it.
  The open chat travels on the action rather than being looked up,
  because the daemon does not know it — two windows can have different
  conversations open, and a header button is about the one the person pressing
  it was looking at.
- **A plugin's text field commits on Enter *and* on losing focus, and says so
  while it has not.** It used to commit on Enter alone, and nothing on screen
  said so: somebody typed a new keyword into the autoreply's box, saw it there,
  closed Settings, and the plugin went on answering the old one — then a
  restart drew the old one back, which read as a setting that had not been
  saved, because it had not. Committing on every keystroke is still wrong (one
  request per letter, and a keyword the plugin is halfway through being given),
  so the box commits when Enter is pressed and when it is left, and draws "not
  saved yet" with a Save button under itself while what it holds differs from
  what the plugin was last given. That last phrase is the subtle half: the
  window remembers the value it *sent* as well as the one the plugin
  *published*, because a plugin may store without redrawing — compared against
  the published value alone the line would never go away — and because Enter
  followed by a click elsewhere is two commits of one value, of which the
  second must find nothing pending. The plugin publishing a different value
  clears that memory: its answer replaces the window's guess, and the box.
- **A module the loader refused is published, with the reason.** Loading is
  the one failure whose message used to reach only the daemon's log, and the
  Settings screen filled the gap with a guess — "not a plugin, or built against
  a different version of the ABI" — over a file whose actual problem was the
  flags it was built with. So `Registry::refuse` records the id and the
  sentence as a surface of its own kind (`PluginSurface::refused`, distinct
  from `stopped`: a stopped plugin ran and declared things, a refused one has
  nothing but the reason), and the card draws it. The reason most worth naming
  is a shared memory: the root `.cargo/config.toml` gives the wasm target the
  web front end's flags, cargo joins a target's `rustflags` from every config
  file up the tree and no config *under* the examples can take them back, so a
  bare `cargo build --target wasm32-unknown-unknown` in a plugin's directory
  produces exactly that module. `cargo xtask plugin build <dir>` is the one
  place that clears `RUSTFLAGS`; the loader's message names it, and a test
  reads the message back so wasmi's wording cannot drift out from under the
  match that recognises it.
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
  walked straight past. It answers three things and not two, because the
  sender picks which one it sends: no parameter set is nothing new being
  declared and is left alone, a size is bounded, and a set it cannot follow is
  refused. Folding the last two together made the way past the budget a
  parameter set shaped so the parser gives up — which the peer chooses — and
  the shapes that actually reach it are the hostile ones: a truncated set, a
  `ue(v)` of more than 31 zeros, a frame cycle longer than the bytes carrying
  it. Baseline and main, which is all a call has ever carried, parse.
- **A decoded picture is a slot, not a place in a queue.** The window's event
  channel is hundreds of messages deep because the messages that may not be
  lost need it to be, and a decoded 720p frame is 3.5 MiB — so frames put
  there would let a stalled window bank gigabytes of obsolete video *and* park
  every state frame behind ten seconds of it. `LatestFrames` holds one picture
  per direction, the newest overwriting the last, and the channel carries only
  a nudge; a dropped nudge costs nothing, because the slot still holds the
  newest picture and the next frame nudges again.
- **A picture's position is where it is shown, not where it was decoded.**
  An attachment has two orders — `stts` says when a sample is decoded and
  `ctts` the offset to when it is displayed — and they differ exactly when a
  stream carries B-frames, because a picture referencing a later one has to
  be decoded after it and shown before it. So every index above
  `video::demux` is a **rank** in presentation order: a seek target, a
  position on the scrubber, `StreamingFrame::index`, and the stamp a unit is
  fed to WebCodecs under. Only the two feed loops count in decode indices,
  and `Track::decode_index_of` is the one place the orders meet. Stamping a
  unit with where it was *fed* is the bug this replaced: a browser answers in
  presentation order and hands the label back with the picture, so the
  answers arrive out of sequence and the timeline reads a picture as a
  position it does not hold. Invisible until it is not — decode order *is*
  presentation order on every baseline stream, which is every video WhatsApp
  itself sends, and only an attachment from somewhere else has the shape that
  breaks it.
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
- **The peer's picture is asked for from exactly two places, and both are
  above the library.** A dropped access unit used to end the peer's stream for
  good: the decoder abandons its chain at the gap and waits for a keyframe the
  peer sends only on its own cadence, which for WhatsApp mobile is on demand
  and therefore never. The library parsed an inbound PLI to drive our encoder
  and built none of its own, so the receive path had nothing to say "send me a
  recovery point" with. It does now (`CallHandle::request_peer_keyframe`,
  oxidezap/whatsapp-rust#1385), and what matters here is *who is in a position
  to notice a loss worth asking about*.

  Not the library: it hands each access unit over intact, so by the time one
  is lost it is above the hand-off and invisible below it. `pump_remote` is
  where it actually goes -- the window's queue refuses it -- which is why the
  ask is made there and not deeper. It fires on every dropped unit rather than
  the first, because a window that sheds sheds a run and coalescing a run into
  one request is the engine's throttle, not this pump's job.

  The other is `<video state="1">`: the peer's camera coming on mid-call
  resumes an encoder already running, so the first unit it sends need not be a
  keyframe at all. This is the mirror of the `peer_can_receive_video` ask
  beside it, which asks *our* camera for the same reason in the other
  direction.

  Both pass `KeyframeUrgency::Coalesced`. `Immediate` is for a decoder that
  has already failed and reset -- the front end is what would know that, and
  nothing here reports a decode failure back, so there is nowhere to call it
  honestly yet. The official client draws the same line: `pli_throttle_time_ms`
  for the routine case, `enable_pli_for_dec_err` for the one that skips it.
- **Video encoded before the peer accepts is thrown away twice, and poisons
  the channel it is thrown away in.** The camera has to open before the offer
  -- an offer with no camera is not a video offer -- but nothing wants those
  frames. The window has no live call to draw them into, and the peer opens
  its pane off the announcement sent at accept, not off the offer, so a unit
  arriving before it is decoded by nobody. What made this expensive rather
  than merely wasteful is where the bytes go in the meantime: the relay
  channel is allocated when the server acks the offer, while the callee is
  still ringing, and SCTP starts in slow start with a congestion window of
  about 4 KB. Half a second of 720p at 1980 kbps arrives into that. One call
  logged the channel 138872 bytes behind before the peer had answered -- 561
  ms of encoder output, queued in front of every packet that would matter.
  The plane hand-off is gated on the same flag the self-view uses.
- **A requested keyframe is not free, and four things request them without
  knowing about each other**: the peer's PLI, the media plane's gate, an
  access unit the relay refused, and a self-view frame the window could not
  take. Each is *caused* by congestion, so answering every one individually
  spends the whole bitrate budget on the largest frames the encoder can make
  at the moment the wire has least room for them. One call answered 38 in
  twelve seconds -- better than one frame in seven was an IDR, on a budget
  that affords one in sixty. Requests are now rate-limited to one per second
  in both backends, against the relay's drain time rather than the round trip:
  at 1980 kbps a channel takes about a quarter-second to clear a keyframe, so
  a second IDR inside that window cannot be delivered whatever it costs to
  make. The request is *kept* rather than consumed when it is too soon, so the
  last ask of a burst is never the one lost, and `KEYFRAME_SECONDS` remains
  the backstop.
- **The outbound ceiling is video's, and it was dropping the voice.** One
  Opus stream is 16 kbps against video's 1980, so audio can never be the cause
  of a backlog and is what a call least affords to lose -- yet a video
  keyframe filling the buffer took the voice with it, because both were
  decided against the same line. Audio is exempt now up to a hard ceiling
  eight times higher, past which the channel is not congested but wedged.

- **A video call announces its direction; the offer only advertises the
  capability.** A call placed as video enables its plane ungated, encodes, and
  packetises — and the peer shows nothing, because the receiving side brings
  its video stream up off a `<video state="1">` *announcement*, not off the
  offer. The official client's own decoder is driven by `handle_peer_video_enabled`
  and `update_video_info`, both fed by `<video state=…>`. Android says its own
  direction (`state="11"`) and, when nothing answers for ours, gives up
  (`state="0"`). A mid-call camera already announced, through `start_video`;
  the from-start path was the one that never did.
- **Neither encoder may go without a periodic IDR, and it is not a quality
  setting.** The media plane drops every access unit that is not an IDR while
  one of its keyframe gates is closed — the engine's `keyframe_required` and
  the driver's send gate — and both close on ordinary events: shedding under
  backpressure, a relay reconnect, an inbound PLI. The only notice either
  gives is `CallEvent::VideoKeyframeNeeded`, which the client ignored, and the
  library's own retry logic is written against an encoder that produces an IDR
  anyway. The desktop met that contract with openh264's three-second intra
  period and so never showed the fault; the browser encoder emitted a keyframe
  only when asked, so the first missed request stopped its video for the rest
  of the call. `KEYFRAME_SECONDS` is now one number both backends read, and the
  event is answered as well — the cadence bounds the outage at three seconds,
  the answer ends it in one frame.
- **The outbound ceiling drops whole access units, never part of one.** The
  browser relay's queue ceiling was applied per packet, which is right for
  audio and ruinous for video: one Opus packet is one frame, but a 720p IDR is
  tens of fragments and is itself large enough to cross the ceiling *while it
  is being written*. What reached the peer was a keyframe with a hole in it,
  and so was everything referencing it. Worse, the transport returns `Ok`, so
  the library believed all of it went out — its own per-unit shedding never
  ran, no gate closed, nothing asked for a replacement. The verdict is now
  taken once, at an access unit's first packet, and holds to its marker bit; a
  unit already begun is finished whatever the queue has done since, because
  the bytes are spent either way and spending the remainder is what makes them
  worth anything.

- **The relay reports which RTP streams crossed it, not just that media
  did.** `RelayPacketKind::Rtp` says "media" and stops there, so a call
  sending audio alone and one sending audio and video produce the same three
  words in a release line. That gap cost a whole round: the outbound video
  path was instrumented end to end, every stage of it proved to work — 276
  chunks encoded, 269 handed to the media plane — and the peer still drew
  nothing, leaving exactly one question ("did those packets reach the wire?")
  that nothing could answer. The payload type is the low seven bits of RTP's
  second byte and is fixed for the life of a stream, so the *set* of them is
  the entire answer and costs one comparison per packet. Kept for both
  directions, because "we sent one stream" and "they sent us two" are
  different sentences about the same call.

- **The outbound video path accounts for every hop, because all five of its
  failures are silence.** A call whose peer draws nothing can have stopped at
  the capture tick (which declines for three separate reasons), at an encoder
  that configures and then emits nothing, at the queue from the encoder to the
  session, at the queue from the session to the media plane, or nowhere at all
  — the frames went out and the peer could not decode them. Every one of those
  read identically in a production log: not one line, with the camera
  reporting itself open at 1280x720 the whole time, exactly as the relay read
  before #62 and #70.
  So each hop says its first — the first frame submitted, the first chunk out
  of the encoder, the first frame reaching the session, the first handed to
  the plane, the first drawn in the self-view — and the camera's close reports
  the totals. Firsts and totals, never a line per frame: twenty a second is
  not a log. The three tick refusals explain themselves once each and are
  counted after that, for the same reason.
  What the shape buys is a *bisection*. "First chunk" absent means the encoder
  never answered. Present, with "handed to the plane" absent, means the
  session threw it away. Both present and a peer with no picture means the
  fault is downstream of everything this tree owns. Guessing between those
  cost three changes on the relay before the marker was added, which is the
  argument for adding it here before the fourth.
  The self-view is on the same path and fails separately: `NoSubscriber` is
  the ordinary state of a daemon with no window *and* the whole explanation
  for a call that draws the peer and leaves this side blank. It is a front end
  that did not subscribe, not a camera that failed, and now it says so.

- **The browser's camera reaches WebCodecs through a `<video>`, and that
  element has to be in the document.** A hidden element plays the stream and
  every tick takes a `VideoFrame` from it. `MediaStreamTrackProcessor` would
  read frames off the track with no element at all and is the nicer shape, but
  it is not in every engine this has to run on — Firefox has none of it — so
  the element path has to exist regardless, and one path is better than two.
  Re-check the support table before concluding it still has to. It was written
  *detached*, on the reasoning that an element with no parent still decodes
  and an added one would draw the self-view twice. Production disagreed, on
  every camera a call ever opened: `play()` rejected with "The play() request
  was interrupted because the media was removed from the document." Blink
  decides that on `InActiveDocument()`, which is `isConnected()` and an active
  document — a never-inserted element fails it exactly like a removed one. So
  the element is appended, one pixel of it, off screen and fully transparent.
  Not `display: none`: a hidden element is entitled to stop rendering, and
  this one exists to produce frames.
  The second half is that `play()` is not the question — whether frames will
  come is. The two came apart here: the promise was aborted for a lifecycle
  reason while the element went on decoding, and treating the rejection as
  fatal downgraded every video call to voice. The element is asked directly
  instead, and asked whatever the promise did — a rejection, a resolution, and
  a promise still pending when the grace expires all reach the same test,
  since asking only on a rejection lets the one case that never answers
  through untested. The test is `paused` first, then `readyState` and
  `videoWidth`. `paused` is the
  load-bearing half — a `MediaStream` reaches `HAVE_CURRENT_DATA` with a
  nonzero `videoWidth` the moment the element is wired to it, whether or not
  it was ever allowed to start, so readiness alone would pass a genuine
  autoplay refusal and then feed the encoder one still picture for the length
  of the call. Playing *and* showing something is the question; either half on
  its own is not.
  And a media element playing a `MediaStream` is rooted by the *browser*,
  because playback is a root: one merely dropped goes on being a sink on the
  camera's track for the life of the page. Six refused attempts in one call is
  six of them. Paused, unwired and removed, in that order, it holds nothing —
  which is `release_element`. Every exit reaches it: `attach` on its own
  refusal, `Held` at the end of the call, and `ElementGuard` in between, since
  the element is inserted and playing three fallible steps before `Held`
  exists. That guard is the same shape as the camera's and the encoder's
  beside it, and for the same reason — the leak is not on the path anyone
  looks at.
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
- **A page has no fonts but the ones it embeds, and the failure is a panic.**
  A desktop's text system reads the system's families through font-kit; the
  web backend builds `CosmicTextSystem::new_without_system_fonts`, and a
  browser hands wasm no font files, so the database starts empty. `gpui_web`
  used to fill it — it bundled IBM Plex Sans and Lilex and added them as it
  built the platform — and a revision bump took that out with the note that
  applications must add their own before opening a window.
  `platform/fonts.rs` is that answered, and the two families are decided
  upstream rather than by taste: `font_name_with_fallbacks` maps `.ZedSans` to
  "IBM Plex Sans" and `.ZedMono` to "Lilex", and the web platform passes "IBM
  Plex Sans" as what `.SystemUIFont` resolves to.
  What it looks like when it is missing is worth remembering, because none of
  it names a font. `resolve_font` *panics* when neither the family nor a
  fallback resolves, so the first frame traps — and a wasm trap unwinds
  nothing, so every `RefCell` gpui held across that frame stays borrowed for
  the life of the page. The console fills with "RefCell already borrowed" from
  gpui's own window and async context, at a rate of several a second, while
  the session behind it connects, hydrates, authenticates and syncs perfectly.
  One panic naming a font, then hundreds naming a cell.
  It is held by `cargo test` rather than by a browser, and honestly: the tests
  in that module build a `gpui::TextSystem` over the very same
  `new_without_system_fonts` with the very same system-font name, so a family
  a page cannot resolve is one they cannot resolve either — including the one
  that proves the *fallback* still lands, since gpui-component's default mono
  family here is "DejaVu Sans Mono" and no page has that either.

- **The web build is a profile, not a `cfg`.** `[profile.release]` is
  calibrated for the binary, where an optimization is paid for once and
  collected at every frame after; the web artifact is one module a visitor
  waits on before the first pixel and a browser then compiles, and its code
  section is 84% of it. Cargo has no per-target profiles, so `[profile.web]`
  is the answer and `cargo xtask web build` selects it through trunk's
  `--cargo-profile`. `opt-level = "s"` there was measured at 31% of the module
  — by a wide margin the largest single thing in it, larger than every crate
  gate put together. `gpui` is the one exception at 3: it draws every frame
  and is the largest crate here — and it has to be named there, because a
  profile replaces its parent's *base* setting, so a crate the sweep does not
  mention is at "s" here and at "3" on the desktop. Package overrides, on the
  other hand, **do** inherit through `inherits`: cargo merges the parent's
  package table into the child's and lets the child's entries win. This file
  and the manifest both said the opposite for a long time, and the table under
  `[profile.web]` had grown to 46 entries of which 39 were repeating the
  desktop sweep and doing nothing. Reproduced rather than reasoned about, on
  cargo 1.98: `cargo build -p url --profile web --target
  wasm32-unknown-unknown -v` compiles `url` at `z`, the level
  `[profile.release.package.url]` names, under a profile whose own base is "s"
  and whose table does not mention it. (`-p` takes any package in the resolve
  graph, not only a workspace member, which is what makes that a two-second
  check rather than a build of the window; it needs
  `rustup target add wasm32-unknown-unknown` on whatever toolchain runs it.) So `[profile.web]` holds the
  differences and nothing else, and the desktop sweep — `ureq`, `zbus`,
  `wayland-*`, `libsqlite3-sys` — costs this graph nothing where it names
  crates that are not compiled for wasm at all.
  Two ways in that look like they should work and do not:
  `CARGO_PROFILE_RELEASE_PACKAGE_<NAME>_OPT_LEVEL` is silently ignored, and
  `--config`, which is not, is not something trunk can forward.
- **A source map for wasm is a projection of DWARF, and its columns are file
  offsets.** The module is treated as a single line of text whose columns are
  its bytes, so `xtask/src/sourcemap.rs` sorts `.debug_line` by address and
  emits one segment per byte. The one adjustment in it that no specification
  states is that DWARF's addresses are relative to the *code section's
  payload* rather than to the file, so every offset has that section's start
  added to it — measured against a module built for the purpose, where the
  single function's first instruction is at file offset 110 and DWARF calls
  it 3, against a payload beginning at 107. Two more things are load-bearing
  and neither is obvious. The build may not be run through wasm-opt: it moves
  code, and it updates the line table only for the transformations it knows
  how to follow, so `-Oz` produces a table that still parses and no longer
  describes the module — a debugger confidently naming the wrong line, which
  is worse than one naming nothing, because nothing about it looks broken.
  And the rows the linker discarded are not removed but pointed at all-ones,
  so a generator that maps them puts every dead function's source lines over
  whatever really is at the end of the module.
  Three smaller things the two formats disagree about, each of which is one
  character or one function's worth of wrong and none of which looks wrong:
  DWARF numbers columns from one and a source map from zero, and they agree
  only about zero, which DWARF spends on "the left edge of the line"; where
  several rows share an address it is the *last* that describes the code
  starting there, the ones before it covering no bytes at all, so keeping the
  first names the line before at every inlining boundary; and a sequence's
  end row names no source on purpose — a source map's lookup takes the
  segment at or before the offset, so the only way to end a range is to start
  one that names nothing, and dropping those lets a function's closing line
  answer for the padding and the function after it. Only sources under the
  checkout are embedded in the map: naming the standard library's files
  costs nothing and carrying `build-std`'s copy of them is most of a
  gigabyte of JSON to answer a question a file name has already answered.
- **Names and lines are different sections, and profiling reads the first
  one.** A flame chart names a wasm frame from the *name section* — that is
  what the Rust and WebAssembly book's own profiling chapter says, and what
  DevTools falls back through: the name section, then import and export paths,
  then a `$func123` off the index. So `WEB_PROFILE=debug` is the profiling
  build and always was, and the source map buys nothing there. What the map
  buys is the Sources panel: a panic's stack trace naming a file and a line, a
  breakpoint, a step. The one document that pairs source maps with a flame
  chart is about *minified JavaScript* and never mentions wasm, and Chrome's
  own wasm debugging page covers the Performance panel and the DWARF extension
  without mentioning source maps at all — which leaves the negative unproven,
  since nothing states that the Performance panel ignores a wasm map. The
  advice rests on the mechanism rather than on a promise. `web-dwarf` keeps
  the name section as well (`strip = "none"`, and no wasm-opt left to drop
  it), so it can answer both questions; it is still the wrong build to time,
  because skipping wasm-opt is a different code layout and a profile of it
  measures a program nobody runs.
- **A size override is worth what a crate weighs *after* LTO.** Which is not
  what it weighs in the sweep, and the two are not even correlated — so the
  order is measure, then decide, and `cargo bloat --crates` against a build
  with `CARGO_PROFILE_RELEASE_STRIP=none` is the whole of the first half.
  Measured on this tree, each figure from a build whose only difference is
  the entry being measured: taking the image formats `gpui` turns on that
  nothing here can name — `exr`, `tiff`, `qoi`, `color_quant` and the
  `zune-inflate` under `exr` — from the profile's setting down to `z` is
  worth **32,494 bytes** of a 22.7 MB module, because fat LTO had already
  removed nearly all of it and what is left is *data* that no optimization
  level shrinks (`exr`'s DWA transfer curve is 131,076 bytes of it, in the
  window's `.data`). Which format is reachable is a question to answer from
  `utils::mime_to_image_format` rather than from the crate's name: a decoder
  is *named* there, not sniffed for, and GIF is one of the six names it can
  answer with — so `gif` belongs with the codecs kept at `s`, and the first
  draft of this had it in the list above.
  Which is the smaller half of the lesson. The larger one is that "only X
  reaches this crate" is a claim about the dependency graph, and
  `cargo tree -p <bin> -i <crate>` answers it in a second — where reading the
  crate's name and imagining its callers gets it wrong about a third of the
  time. Every "reached only by" in this manifest was written that way once,
  and four were false: `gif` is decoded here; `rayon` is `sum_tree`'s as well
  as the decoders'; `aho-corasick` is a *direct* dependency of `gpui-base`,
  whose editor search builds one as a person types; and `moxcms` is reached
  from `image` itself for any picture carrying an ICC profile. Ask the graph
  before writing the sentence. Taking `waproto` and `buffa` from `3` to `z`
  is worth **1,226,368 bytes** of the daemon, because generated protobuf
  survives LTO in full: it is reachable, it is enormous — four separate
  72 KiB copies of `Message::clone` among the largest functions in the
  binary — and none of it is in a loop. The cold-and-obvious crate is
  usually already gone; the one worth finding is large, reachable, and
  called once per stanza rather than once per frame.

  Both of those numbers were wrong when this paragraph was first written, and
  wrong the same way: they were the totals of a change set in which a dozen
  crates moved at once, written down as though they belonged to the one entry
  the sentence was about. It read as 43 KB and 1.4 MB; isolated, it is 32 KB
  and 1.17 MiB — the protobuf really is 82% of that sweep, and the image
  formats really are nothing, so the story survived, which is exactly why
  nobody would have gone back to check. **A number is about the difference
  that produced it.** One build with one entry changed, or say out loud that
  the figure is a total. The same trap has a second mouth: a "before" from an
  older commit measures the intervening work as well, which is how this
  branch once reported the module *growing* by 380 KB when it had shrunk by
  662 KB.
- **The page has a third heap, and it is the size of the account.** The
  relaxed-idb VFS holds `HashMap<usize, Uint8Array>` — the whole database,
  resident in the *JavaScript* heap, one 8 KiB page per entry, kept alive
  through wasm-bindgen's object table. A snapshot of a logged-in session
  showed 1,528 of them: 12 MiB of database beside 7.6 MiB of linear memory,
  under 32 MiB of V8-compiled module. So "the wasm heap" is not where the
  store's memory is, the budgets in `session/attach.rs` do not
  bound it, and it grows with history rather than with what is on screen.
  Another argument for OPFS in a worker, and a larger one than durability.
- **A frame may not cost what the conversation costs.** The conversation pane
  reads the selected chat and then needs the app mutably to build the
  timeline, so what it takes has to survive that — and a `Chat` taken by value
  is its messages, each of them four `String`s, a reaction map, a quote and a
  media handle. `chats` holds `Arc<Chat>` for that reason and every write goes
  through `Arc::make_mut`, which costs nothing while the only other holder is
  a frame about to end. The same rule reaches the rows: `BubbleProps` carries
  an `Arc<ChatMessage>`, and the four to seven element ids a bubble draws
  under are formatted into `MessageListCache` when the rows are built rather
  than per row per frame — that cache is already rebuilt exactly when the
  messages change, which is exactly when an id could differ. The text goes
  the same way and for the same sentence: `BubbleText` is the markup already
  resolved, so a bubble no longer clones `content` into a `SharedString` and
  parses it — a scan of the peer's message and the partition its spans
  resolve to — for every visible row of every frame. What it is *not* is the
  appearance: a `HighlightStyle` is built against `cx.theme()` and the
  metrics, either of which can move under a timeline nothing else
  invalidates, so the parse is cached and the styling is the asking frame's.
  `app::frame_cost` is what holds all of it: a counting allocator, ignored by
  default, asserting that the per-frame path does not scale with the
  conversation behind it. It counts allocations rather than milliseconds
  deliberately — the machine with the problem is a browser running
  `dlmalloc`, and a count is the same count on both.
- **A wait is a call across the boundary, and there are three of them.** Every
  `setTimeout`-as-a-future in the tree — the window's clock, the library's
  runtime clock, a plugin's scheduler — used to ask `web_sys::window()` to arm
  and ask again in its guard's `Drop`, and then `clearTimeout` a handle the
  browser had already retired. That is an `instanceof` plus two calls per
  *tick* of every loop the page runs, and the loops are not rare: the
  library's `yield_now` is its clock at zero milliseconds with a
  `yield_frequency` of one, so a history sync arms one per message. The
  global is resolved once per agent into a thread local, and the callback
  raises a flag the guard reads, so a wait that ended the ordinary way
  cancels nothing. `try_with` rather than `with`, because these guards are
  dropped from tasks that can be torn down while thread locals are being
  destroyed and a panic there is a panic in a destructor.
- **Decoded images are cached by message id**, because GPUI tracks animation
  state per `Arc<Image>` and rebuilding one re-decodes the bytes. Whoever
  replaces a preview with real bytes must evict the entry — and so must
  whoever *takes* the bytes away, which is the sweep below.
- **A conversation lets go of media it can fetch again, and only that.** A
  message holds its own bytes for as long as the row is loaded, so the two
  media budgets that exist bound what is *cached* rather than what the window
  is retaining. `Chat::release_media` is the arithmetic and it lives in
  `oxidezap-core` because the ordering and the budget are about the data; the
  *judgement* is `WhatsAppApp::sweep_retained_media`, because a viewport is a
  front end's and core has none. Three things make it safe rather than
  destructive. Only bytes with a `downloadable` beside them go, so a voice
  note recorded here or a poster frame that is the row's only picture is
  never "evicted" into deletion. What the interface is holding open —
  playing, in the viewer, mid-download — is pinned whatever the budget says.
  And a released row is left in the state a row whose media never arrived is
  already in, which is why it costs no protocol change and no new field: the
  renderer already draws that as an offer to download, and the press that
  accepts it is the same press it always was.
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
- **A capture the microphone refused still goes through the resampler.**
  `stop` answers with what it has, and what a denied `getUserMedia` leaves is
  no samples and no rate — the rate is learned when the device opens. The
  reason for the refusal is read *after* the capture has been prepared, so
  that a caller which abandoned the recording is told nothing at all, and
  zero is a whole multiple of 16 kHz: the preparation took the decimation
  branch and divided by a step of zero. On a page a panic is the end of the
  tab, not of a thread, so `resample_to_16khz` answers an empty capture with
  an empty one before it looks at the rate.
- **Preparing a note and encoding it are two steps, and only one of them may
  leave the window.** Resampling to 16 kHz and measuring the envelope is pure
  Rust on both platforms and the expensive half — a 63-tap filter over as
  much as ten minutes of audio — so `RecordedAudio::prepare` is a step of its
  own and `app/recording.rs` hands it to the background executor on both. The
  codec is what differs: libopus follows the preparation onto the same
  worker, while `AudioEncoder` belongs to the document that made it and is
  awaited on the window. `Recording::Pending` is that seam — the capture out,
  the prepared note in, the encoded note back — which is why the front end
  reaches one `finish` with no `cfg` in it. Splitting the *resampler* instead
  is the wrong half to reach for: a 63-tap filter carries state across any
  boundary it is cut at, so chunking it changes the audio.
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
- **Reclaiming abandoned media is a schedule, not a side effect of a write.**
  A `w-` (a download in flight), a `u-` (a payload staged for a send) and a
  `.staging-` partial are all spared the budget sweep, because the bytes are
  the only copy or are not all there yet, so each needs an age rule run
  somewhere or being spared means never being collected. That rule used to run
  from `prepare_dir`, under a comment saying it ran once — but `put` is
  `prepare_dir`'s only caller, so it walked the whole directory on every cached
  byte range, with the wipe lock held, which is precisely what
  `SWEEP_INTERVAL_BYTES` exists to prevent. The daemon runs it as a task now:
  once at startup, then every `RECLAIM_INTERVAL`, on a thread of its own. A
  hook at any of the places an orphan is *made* would not do — the desktop
  front end stages a payload by writing the file itself, so the daemon never
  sees it happen.
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
- **A tagged enum has no room for an array, and it says so at run time.**
  Every enum on this wire is internally tagged, so a variant is written as a
  map with the tag beside its fields — which means a newtype variant is only
  writable when its payload is *itself* a map. `PluginsChanged(Vec<_>)` was
  not: serde refuses a tagged newtype variant containing a sequence, so every
  one of those frames failed at `to_string` and the daemon dropped it with a
  log line, from the version that introduced plugins until the one that
  fixed it. Nothing in the type system has anything to say about this, which
  is half of why it shipped; the other half is the snapshot, which carries
  the same set and so made a window attaching *after* a change look right,
  losing only a change made while it watched. What that is, exactly, is
  somebody approving a plugin: the answer was recorded, the set republished,
  nothing drawn, and the switch flipped back. So a variant carrying a list
  takes a named field, and `every_daemon_event_survives_the_wire` is the
  question asked of every variant by serializing it — exhaustively matched,
  so the next one added cannot skip it.
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
  were waiting for it, and `Chat::author_name` is what every surface asks —
  the bar above a reply included, which is a surface and not a special case.
  It had been left to the conversation alone: the participant map only holds
  whoever has a row on the loaded page, so a reply to a message that was
  never loaded read "Unknown contact" over a group whose bubbles from that
  same person, named from the book, read fine. `hydrate_quoted_authors` asks
  the book for the quote's author the way a bubble's is asked for, and leaves
  this account alone so `Chat::quoted_author` can still say "You". A
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
  `StoreChange` by re-querying, so emitting one for a batch that wro