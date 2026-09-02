# The web front end

The page runs the whole client: the session, the store and the window. It can
attach to an `oxidezapd` the visitor runs instead — over the same protocol the
desktop window speaks, and worth preferring, since a desktop daemon holds
calls, keeps plugins, survives the tab and keeps the keys out of a browser's
storage — but it no longer needs one. The export stays static either way: nothing here needs a
server to be *hosted*. `.github/workflows/pages.yml` builds and publishes it.

The same bundle ships in every release as `oxidezap-<version>-web.zip`, built
by `.github/workflows/web-bundle.yml` — so hosting it somewhere else is
unpacking a directory rather than installing a nightly toolchain and trunk.
The one difference is the public URL: Pages knows its own directory and bakes
it into the generated glue, and an archive cannot, so that build is told `./`
and every asset is named relative to `index.html`. Which is why it is a second
build rather than a copy of the Pages artifact, and why the workflow asserts
the relocatability rather than trusting it — an asset named from the origin
root is a bundle that only works unpacked at a domain's root, and that is the
one way `--public-url` can silently come out wrong.

The daemon a page runs is the daemon, minus the process:
`daemon::embedded::start` assembles the state hub and the session bridge and
hands the front end one end of a `tokio::io::duplex`, which `serve_client`
already accepted — so the page speaks the same frames down a pipe that the
desktop speaks down a socket, and not one line of protocol is written twice.

**Plugins are the daemon's, so a page gets whichever daemon's it is talking
to.** Attached to an `oxidezapd`, all of it: the web bridge hands
`serve_client` the same `Plugins` the socket does, so a plugin's interface
arrives in the snapshot, its buttons act through `PluginAction`, and its
permission prompt is answered through `PluginApproval` — not one line of that
is a second implementation, because the protocol already carried it.

Holding its own session, its own — and that is the same sentence rather than
an exception to it. A page's daemon runs the same host, over the same
sandbox, with the same bounds and the same protocol underneath; what differs
is where three things come from, and each is a platform split inside the host
rather than a second host. A plugin gets a task on the page's loop instead of
a thread (`plugin-host/sched/`), which is what the `async` shape of the worker
loop is for — on a desktop every call in there blocks, because the future is
driven by a `block_on` on a thread with nothing else on it. Its module comes
out of OPFS instead of a folder (`daemon::plugins::web`), which is a real
directory whose listing *is* the registry, exactly as it is on a desktop: the
file's name is the plugin's id. And its approval and its settings come out of
`localStorage` instead of a private directory (`plugin_host::Origin`), because
both are read and written from inside a synchronous wasm call and an
asynchronous store would have to be mirrored in memory and written behind the
caller's back.

What replaces `only_this_user_can_write` there is the origin itself: an
origin's private filesystem is reachable by that origin and by nothing else,
which is a stronger sentence than a `0700` directory makes and one the browser
enforces rather than this code. What it does not answer is the same thing a
folder does not answer — that the module is the one the user meant — which is
what the approval prompt is for, unchanged.

The one thing a page cannot order by waiting is a plugin's *last* write. A
desktop joins every plugin's thread before it replaces the host, so the
settings write has already happened; a page cannot join a task on its own
loop, and a worker not polled since the shutdown flag went up still has that
write in front of it. Two things it would land on: after a wipe it recreates
the departed account's data under whoever pairs next, and after an ordinary
reconnection — no wipe at all — it puts the old host's in-memory settings over
what the new host has already written. So a store is stamped when it is taken
and an older handle's write is refused, `Origin::storage` and `forget_all`
both moving the stamp on. *Superseded* rather than a latch, because a page
rebuilds its whole service in the same agent: a latch would leave the new host
unable to write for the rest of the tab's life — grants rolled back, settings
lost — while the tasks it was aimed at were the old host's.

Retiring is where a page fails closed rather than tidily. A browser that
refuses `localStorage` outright is not the same fact as an origin that never
held an approval, and nothing here can tell the two apart — so `forget_all`
answers `false` and the wipe is refused, because a storage context that is
shut can be opened again and the approvals it still holds would then be read
back for whoever paired in the meantime.

What a page draws about that folder is two lists rather than one. A module
that fails to parse, answers the wrong ABI version or traps in `oxi_init`
publishes no surface at all, so Settings drawn from the surfaces alone leaves
the one file somebody most needs to remove with no control anywhere — and it
goes on spending the folder's budget at every load.
`ClientRequest::ListInstalledPlugins` is that second list, asked when Settings
opens and again after an install or a removal, the same shape and on the same
terms as the storage total beside it.

Adding one is a request too — `ClientRequest::InstallPlugin`, with the module
staged through the media cache under a `u-` key exactly as a file being sent
is, because a `.wasm` is up to thirty-two megabytes and a request frame is
capped at one. It was not always: a page holding the session had the daemon in
its own address space, and the window called `daemon::plugins::web::install`
directly. That was a second control channel beside the protocol, it existed on
no other target, and it is why a desktop window's Add button did not exist at
all. The folder belongs to whichever daemon runs the plugins, so every front
end now asks that daemon — including the one that could have reached past it.

The front end still says which folder it is looking at rather than guessing:
`platform::plugins::home` is the mirror of `daemon::plugins::start`, and the
two halves are written to be read together — a page that drew "drop a .wasm in
the plugins folder" would be giving instructions about a folder it does not
have. What it decides now is the *sentence*, not the controls: a tab that
holds no session installs perfectly well and cannot start what it installed,
because the folder is one per origin and the host is one per account.
Installing *does* start it, by asking the daemon to reload — one act from
where somebody is standing and two on the wire, because installing and
loading are two different moments and a reload retires the whole generation.

**A page picks files the only way a page can.** There is no path to a
filesystem and no `showOpenFilePicker` worth depending on — it is Chromium-only
and wants a secure context a developer's `trunk serve` may not have — so
`platform::picker` builds an `<input type=file>`, joins it to the document for
the length of the gesture, and takes it out again: a detached input's `click()`
is ignored outright by some engines, and one that stays is a control the page
grew and never lost. It waits on `change` *and* `cancel`, because a browser has
two ways of ending this and waiting only for the first leaves the task and its
closures alive for the life of the page every time somebody changes their mind.
The type comes from `File.type` where the agent recognised it and from an
extension table where it did not, which is the same table the desktop half has
nothing but. What comes back is bytes, on both platforms, because what happens
next is a staged upload.

**Media crosses the bridge in both directions.** The daemon's web endpoint
served media and nothing else, so a page attached to an `oxidezapd` could read
a photo and hand it nothing: `MediaCache::stage` refused, and a voice note
recorded there would have failed at the staging rather than at the send. The
mirror route is a `PUT` narrowed three ways, because a write endpoint on the
process holding the account deserves more than a read one — only `u-` keys, so
a caller cannot replace the bytes behind a photo already drawn out of the
daemon's own cache; a declared length, since the length decides how much is
read; and a ceiling checked against it before a byte arrives, because unlike a
served file this payload is read into memory whole.
Ordering is the harder half and it is `stage_then`: the daemon opens the
payload when it handles the request, so a frame that overtakes its own upload
names a file that is not there. The continuation therefore belongs to the
implementation — it runs before returning wherever staging is a local write,
and from the upload's own completion where it is not — and the request id is
still reserved in the order the person acted in. Only the frame waits.
And what it waits *in* is a queue of places rather than a count: two notes can
be staging at once and their uploads finish in whatever order the network
settles them, so a send takes its position when it is made and the upload only
fills it. Counting them instead told whichever finished first that it was the
head of the queue, which is the same bug one level down, record two notes and
let the shorter one land first, and they arrive reversed.
A discard is the mirror and has the same hazard: a `DELETE` issued while the
`PUT` is still crossing can be overtaken by it, leaving the payload staged
with nothing that will ever read it. So a send abandoned mid-upload is
*recorded* rather than removed, and the upload's own completion is what
removes it, one decision, made after the write it is undoing.
And what waits in that queue is a frame *and the reservation it answers for*,
because the connection can end while it waits: `Frames::finish` fails every
reservation and knows nothing about the outbox, and the `Link` it holds is a
clone that does not necessarily refuse a later write. Without the id a line
typed behind a voice note reaches the daemon after the window has already
drawn it as failed. A frame carrying no id is fire-and-forget and writing it
late costs nothing.

**Which tab holds the account is claimed, and the tabs that lose it are front
ends.** `daemon/claim/` is a lock file on the desktop and a Web Lock in a
browser, taken with `ifAvailable` so a tab is told *now* whether it has the
account — the answer decides what it becomes, so it cannot be waited for. What
it becomes if the answer is no is not an error screen. The tab that won is
running `daemon::embedded`, which is a daemon by every definition here — one
session, one store, one writer — and a daemon is something more than one front
end can talk to. So a second tab attaches to the first over a fourth transport
and draws the same account, live, with no handover and nothing disconnected.
That is the whole feature: WhatsApp Web ends one tab's session when another
opens, because there the session lives in the page.

The transport is `ipc/endpoint/tab.rs` and `daemon/listener/tab.rs`, which is
the same two places every other transport lives in, and above them not one line
of protocol is written twice — `serve_client` was already generic over
`AsyncRead + AsyncWrite`, so a connection is one end of a `tokio::io::duplex`
with its lines moved across. What carries them is a `BroadcastChannel` named
after the connection rather than a `MessagePort`, and that is a limitation
rather than a preference: a port is delivered by *transferring* it, and
`BroadcastChannel.postMessage` takes no transfer list. A name only the two
parties use is what stands in — not private, because nothing same-origin is,
but enough that one connection's frames are not delivered to every tab in the
origin. Deriving the channel name from the ask is what removes the race rather
than narrowing it: the asking tab opens the channel *before* the ask goes out,
so there is no window in which the answering tab writes to a channel nobody has
opened.

Media does not travel as a frame there either. A follower has no media map and
no HTTP endpoint, so the sideband is three more messages on the same channel,
with the bytes crossing as a `Uint8Array` — one structured clone, where JSON
would be a base64 round trip through a string twice the size.

Both ends of it run in a browser under `cargo test`, and that is not
belt-and-braces: the leader built its connection handler, held it for exactly
the right lifetime, and never called `set_onmessage`. Everything compiled,
every lint passed, and what a second tab got was a rendezvous answered
perfectly followed by silence — `serve_client` waiting out its handshake
window and refusing a hello it was never handed. The only error anywhere
appeared in the *asking* tab, naming a frame it had sent correctly. Reading
does not catch a call that is not there; running it does, which is what
`listener::tab::tests` is for.

**Queuing for the lock is now the right thing, and the reasoning that ruled it
out has not been dropped so much as spent.** It said a queued tab looks like
one that is starting and would silently take an account nobody was looking at.
Both halves were about a tab that had been *refused*: it was idle, and it was
showing nothing. A follower is neither — it is drawing the account, through the
tab that holds it — so `claim::promotion` queues behind the leader, and the
browser grants it at the moment that tab goes, whatever took it away. That
grant is also the only thing watching: a `BroadcastChannel` has no close event
and a killed tab says no goodbye. The follower ends its connection, the front
end's own retry calls `embedded::start` again, and it finds the claim already
held — by itself. One connection per follower is watched the same way, with
`tabs::liveness_lock_for` held by the front end and waited on by the leader, so
a tab that vanishes is noticed at the moment it vanishes and nothing anywhere
polls.

Being handed the lock is not the *only* way a follower learns its leader has
gone, and it cannot be: with three tabs open, one follower is granted the
account and the others stay queued behind a lock that tab now holds for its
lifetime, over a channel to a tab that will never post again. So a follower
listens to the rendezvous for the whole life of its connection, and a
`Leading` from anywhere ends it — a leader announces exactly once, on the way
up, and a `BroadcastChannel` does not deliver to the object that posted, so
hearing one always means a *new* leader and a connection worth remaking.
The same announcement is why an ask is answered idempotently: a follower
re-asks when it hears `Leading`, and an ask that landed just before that
announcement is one the leader has already served — serving it twice puts two
`serve_client` instances on one channel, where a press sends one message
twice. The nonce is the connection's name, so the name is what is remembered.

A payload's ceiling travels *with* the request, and is enforced by the tab
that has the bytes. It has to be: what crosses is a `Uint8Array` the serving
tab builds and the browser clones, so a ceiling applied on arrival is applied
after the copy it exists to prevent.

`AppState::Refused` survives, for the one case that is still settled: something
holds the account and will not answer for it — a tab left open across a deploy,
speaking a rendezvous version this build does not. It is reached only after
`ATTEMPTS` rounds of ask-then-try, because the ordinary race is two tabs opened
in the same moment: one takes the lock and then spends seconds opening the
store and starting a session before it can serve anybody, and a single ask
would draw "another tab is running this account" over a tab that was four
seconds from answering. Where it *is* reached the distinction is still the
whole point — the error screen is for an outage, and it promises to keep
trying, offers *Work offline*, and arms a countdown. All three are false for a
refusal: nothing was unreachable, nothing is still trying, and *Work offline*
reads a database this window is precisely the one that could not open.

**The store round-trips, and that was measured rather than assumed.** A page
that had never been visited opens the VFS holding 0 files; one that comes back
after the tab closed opens it holding 1. Worth stating because the failure
mode is invisible without it: a browser with no VFS installed opens the
database in memory quite happily, behaves identically all session, and loses
the account with the tab.

What a page cannot do, and says so rather than pretending. Measured on
nightly for `wasm32-unknown-unknown` rather than assumed:

| | wasm |
|---|---|
| `tokio` `sync`/`rt`/`macros`/`io-util` | yes |
| `tokio` `time` | compiles, traps — see below |
| `tokio` `net` (mio) | no |
| `smol` | no — `async-io` is an epoll loop, via `rustix`/`errno` |
| `cpal` | yes |
| `opus`, `openh264` (both C) | no |
| `symphonia` (aac/mp4), `mp4`, `ogg`, `ringbuf` | yes |
| `wasmi` | yes — but see `std::thread` below |
| `std::thread::spawn` | no |

`time` is the row worth reading twice, because it is the one that says
compiling is not the question. `tokio::time`'s clock is
`std::time::Instant::now()` with nothing under it on this target, so a `sleep`
or a `timeout` links, loads, and traps the first time it is awaited — "time
not implemented on this platform", taking the task with it. The session's own
waiting goes through `exec::sleep` and `exec::with_timeout` instead, and
nothing above them names a clock. The same fact reaches chrono, whose
`wasmbind` feature is what puts `Utc::now()` on the browser's `Date`; it is
one of chrono's defaults and this workspace turns defaults off, so it is named
at the root.

`std::thread::spawn` is the row that used to decide the plugins, and it is
worth separating from the one above it: `wasmi` compiles here quite happily —
a wasm interpreter inside a wasm module is nothing unusual — so the
interpreter was never the obstacle. What was, was that the host gave each
plugin an OS thread and a bounded queue it *blocked* on; the same fact r2d2
ran into — twice, since the pool's *management* threads are a second spawn
behind the connection ones, and `scheduled-thread-pool` unwraps that one.
Both are the library's to answer and it does, in
`storages/sqlite-storage/src/pool.rs`: on the web a "pool" is one connection
behind a lock, keeping r2d2's own spelling so the store above it is written
once. The host's answer is the same shape and is in `plugin-host/sched/`: a
task on the page's loop, an async queue, and `setTimeout` where a thread
slept. What a page still does not have is a thread *per plugin*, so a call
that spends its whole fuel budget is a call the page is not drawing during —
bounded by that budget and by `MAX_DUTY` between calls, which is what the
throttle already measured, and the reason both matter more here than on a
desktop.

So voice notes play and record, a video in a conversation decodes through the
browser's own H.264 (`web_sys::VideoDecoder`, bound from Rust like every other
browser API here), plugins run, and calls are placed and answered — the
microphone and speaker through WebAudio, the camera through `getUserMedia`,
the picture encoded by `VideoEncoder` and the media carried to the relay by an
`RTCPeerConnection`.

The relay is the part worth stating precisely, because it reads like a second
protocol and is not. The native transport dials UDP and runs DTLS, SCTP and a
pre-negotiated `id=0` DataChannel over it — and its own comment calls that "the
synthetic-SDP / wrtc dance" reduced to one layer. A browser does the dance
instead: `session/relay/` writes the SDP answer describing the relay and hands
it to a peer connection, which is the same stack with the browser assembling
it. The library takes it through `Client::set_relay_transport_provider`, a seam
that exists upstream for exactly this and answers with a factory per relay
endpoint, since the server names the relay per call.

One fact on that path is a captured constant rather than something the
protocol carries. An SDP answer must name the certificate the far end presents
and a browser enforces the match (RFC 8122); the native transport does not care
and says the fingerprint "is fixed and cosmetic at this layer". *Fixed* is the
operative word, and it is a claim that was checked rather than taken: read out
of `chrome://webrtc-internals` during calls placed on WhatsApp Web itself, the
remote certificate was the same across separate calls that reached *different*
relay addresses, while each tab's own certificate differed. One value, two
endpoints — that is what makes `RELAY_DTLS_FINGERPRINT` a constant and not a
per-call secret. It is not in the `<relay>` block and there is nowhere else to
get it, so a build that lost it would fail every handshake, which is what
`the_fingerprint_is_thirty_two_hex_pairs` is there to notice early.

Whether a page can record is a question about the *browser* rather than about
the build, which is why `can_record()` is a function where `CAN_RECORD` was a
constant: the encoder is `AudioEncoder`, and an older browser may not have it.
Asked before the microphone is offered either way, because a control that is
drawn and then always fails is worse than one that is not drawn. It is now the
*only* thing asked. The composer used to withhold the microphone from a page
holding its own session as well, on the ground that such a page could not
upload what it recorded; the library's buffered `Client::upload` sends the
body through `HttpClient::execute` these days, which is the one method
`BrowserHttpClient` implements, so all three arrangements a page can be in —
a daemon over the bridge, the tab holding the account, its own session — have
somewhere to send a note.

What `stop` hands back is the capture, not a note, and `docs/gotchas.md` has
why. The part that belongs here is where the preparation actually runs, since
it is the one step of a voice note this page places differently from a
desktop and the answer is conditional. `gpui_web`'s dispatcher sends a
background runnable to a pool of `wasm_thread` workers, but only where
`SharedArrayBuffer`, a shared module memory and `Atomics.waitAsync` are all
present — the isolation `web/coi-serviceworker.js` is there to obtain. Where
any of them is missing it falls back to the main thread, through a
`setTimeout(0)` rather than inline. So `cx.background_spawn` is a real worker
in the arrangement this front end is built for and a yield to the event loop
otherwise, and the resampler is bounded by the ten-minute capture ceiling
either way: a stall on the worse of the two, never a hang.

A call's video decodes the same way, through the same module, and obeys the
same stream rules the desktop path does — a decoder born mid-stream waits for
a keyframe, a gap makes it wait again, the peer's parameter set is read before
the decoder is allowed to allocate from it, and their orientation is *undone*
rather than repeated. What it does not have is the thread per direction, and
does not need one: `VideoDecoder` is already asynchronous, so the work the
thread was there to move off the caller happens off it anyway. This was once
reachable only attached to an `oxidezapd`, back when that was the only place
calls happened; `video/call_web.rs` takes a `CallVideoFrame` and builds
a `webcodecs::Decoder` for it, so a page holding its own session decodes call
video too.

The video decoder is worth reading as the shape it is rather than as a
backend swap. openh264 is *pulled* — hand it an access unit, get a picture on
the same line — and `VideoDecoder` is pushed, with the pixel read out of a
frame asynchronous on top of that. What makes the player above survive
unchanged is that playback is already a timer asking for the frame it is
about to paint: a seek feeds the decoder and returns, and the picture lands on
a later ask. `video/geometry.rs` and `video/demux.rs` are what the two
decoders share, which is everything except the decode — the pixel budget, the
rotation, the channel order, the container walk — because a second copy of
those is a second set of answers to drift apart.

Declining was the exception, back when a page could not answer at all, and the
exception is still instructive. Even then it *did* tell the caller to stop
ringing: `client.voip()` and `reject` carry no `cfg` — their stanza builders
live in `wacore` — so what the `voip` feature gates is the media stack and
never the signalling. Which is why declining kept working while answering did
not, and why `platform::capabilities::calls_unavailable` is now the honest
answer to both: it refuses only a browser with no `RTCPeerConnection`.
This module concluded the opposite for a long time, from a real measurement
of the wrong question: enabling the feature for wasm does pull mio and fail
exactly as its comment described, which says nothing about a function that
never needed it. When a comment says something is impossible, reproduce the
impossibility it describes before believing it.

**A fix is not deployed until the service worker agrees.** `coi-serviceworker.js`
is there because cross-origin isolation needs two response headers GitHub Pages
will not set, and the price is that the *document* comes back through it: an
ordinary reload of a published page can be answered out of the browser's cache
with the old `index.html`, which names the old hashed bundle — so a build that
fixed something looks exactly like one that did not. Unregister it (Application
→ Service Workers) and hard-reload, or check the hash in the bundle's filename
before believing a test of the deployed page. `trunk serve` has no service
worker, which is the other reason to reproduce there first.

What it answers is *navigations and worker scripts*, and nothing else, because
those are the only two responses COOP and COEP are read off: a subresource is
governed by `Cross-Origin-Resource-Policy`, which same-origin bytes pass
without a header. Answering the rest was not merely useless — a request a
service worker answers is a different "world" from the one `<link
rel="preload">` fetched in, so the browser matched neither and the page
downloaded the ~30 MB module twice, saying so in the console each time
("cross-world service worker resource mismatch"). Passing a request through —
returning from the fetch handler without `respondWith` — leaves it in the
page's own world, where the preload is waiting for it.

**`spawn_blocking` is a promise about a thread pool a page does not have.**
The daemon's own code is shared, so a `tokio::task::spawn_blocking` written
for the disk compiles perfectly for wasm and panics the first time it runs —
"there is no reactor running" — taking the connection with it. That is how
approving a plugin in the browser stayed broken through a review, a merge and
a production test of everything around it: the desktop has a pool, and the
approval is the one plugin request whose work is I/O. What decides is what the
work *is*, not where the code lives: a file written and renamed must leave the
runtime's thread, and a `localStorage` set is synchronous by construction and
has nowhere to go. `daemon::plugins::approve` is that split, and it is the
same shape as `plugins::start` and `plugins::reload` beside it.

**A cast to a type no engine defines always fails, and fails quietly.**
wasm-bindgen checks `dyn_into` with `instanceof <the declared type>` unless
the binding carries an `is_type_of`, and js-sys declares a few types the
platform has no global for — `js_sys::IteratorNext`, the `{done, value}` an
async iterator answers with, is one. The emitted shim wraps that `instanceof`
in a `try`/`catch`, so the `ReferenceError` for the missing global becomes a
plain `false` and `dyn_into` answers `Err` with the value back: not an error
anybody wrote, and identical for a perfectly good object and a wrong one.
That is how `daemon::plugins::web::entries` shipped a folder listing that
could not take a single step, taking installing, listing and removing with
it, past every review and every green check. A record whose shape is the
whole of it — an iterator step, an options bag — is read with
`js_sys::Reflect::get`; a cast is for something a browser actually has a
constructor for.

Every browser API in the tree is bound through `web-sys`/`js-sys` from Rust:
the WebSocket, `fetch`, `setTimeout`, WebAudio, `localStorage`, the download
anchor. The one piece of hand-written JavaScript is
`web/coi-serviceworker.js`, and it exists because cross-origin isolation
needs two response headers and GitHub Pages will not set them — a service
worker is the only thing that can, and a service worker is a JavaScript file
by definition.

