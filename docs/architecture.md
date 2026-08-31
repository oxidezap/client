# Architecture

The crate map, the theme, and how the interface responds to viewport size.

## Crates

- **oxidezap-core**: domain types (chats, messages, calls, UI events). No UI, no I/O.
- **oxidezap-audio**: capture, playback, Opus encoding, waveforms. cpal; no UI.
  On the web the sound card and the codec are the browser's: playback is real
  (`decodeAudioData` takes exactly the bytes the daemon sends), so is
  recording, through WebAudio for the capture and `AudioEncoder` for the
  codec, and so is a call's mic and speaker — one `AudioContext` for both
  directions, because the browser's echo canceller only subtracts what it
  played itself. `ogg_opus` is the container both platforms write, because only the
  codec was ever missing — which is also why `MediaRecorder` is *not* the
  route in: it produces a container the browser picks (WebM on Chrome, MP4 on
  Safari) where a voice note is Opus in OGG, so it would have meant a demuxer
  to undo it.
- **oxidezap-chat-store**: materializes the library's event stream into chats,
  messages, receipts and an FTS5 search index. Owns its schema and migrations;
  consumes only the library's public event surface. Extracted from
  whatsapp-rust, where it was application logic living in a protocol repo.
- **oxidezap-video**: camera capture and H.264 encoding for calls. cpal's
  opposite number, and now in both senses: a capture backend per platform
  behind one crate — nokhwa and OpenH264 on a desktop, `getUserMedia` and
  `VideoEncoder` in a browser — and the encoder the GUI already decodes with.
  The browser encoder is configured `avc: { format: "annexb" }`, so what comes
  out is what the library's video source already wants rather than AVCC to be
  converted. No UI, and no decode — decoding belongs to whoever draws.
- **oxidezap-session**: the WhatsApp connection: events, sends, store hydration.
  Knows nothing about how anything is drawn, and nothing about IPC either —
  the daemon translates requests onto its methods. Three of its modules are
  platform splits rather than logic — `net/` is the transport and HTTP client
  a page has to supply, `exec/` is where its tasks run, and `video/` is the
  camera — and the calls are a fourth, in `whatsapp/calls/`. Above them the
  session names no platform.
- **oxidezap-ipc**: the wire protocol between the daemon and its front ends,
  plus the blocking client end of the transport (`Endpoint`). No runtime: a
  front end needs one thread to read and a lock to serialize writes, and the
  daemon is the side with thousands of things happening at once. The domain
  types in `oxidezap-core` *are* the wire format; this crate adds the framing
  around them.
- **oxidezap-logging**: how much the client says about itself, and where that
  answer is kept. A crate rather than a function in each binary for two
  reasons. The level has to move while the process runs — `log`'s global
  maximum already does, but an `env_logger` filter built from `RUST_LOG` at
  startup does not, so a level raised in Settings was let through by the macro
  and dropped by the logger underneath, which is a failure with no symptom
  except silence. And both processes have to read one answer: the window and
  `oxidezapd` log about one account, and a choice only the window remembered
  would leave the process holding the session — where nearly everything worth
  reading is written — at `info` for ever. So `env_logger` keeps the
  formatting and the per-module floors and the global level is an atomic this
  crate owns, the store is a config file on a desktop and `localStorage` in a
  page, and the precedence is stated once: an explicit `RUST_LOG` (or `?log=`)
  wins for the run it was given for, then the stored choice, then `info`.
  Choosing a level at runtime always applies, whatever started the process.
- **oxidezap-daemon**: a library and the binary `oxidezapd` around it. The
  library is everything the daemon *does* — the state every front end
  observes, the bridge that turns their requests into session calls, and the
  protocol spoken down a byte stream — and it builds for
  `wasm32-unknown-unknown`. The binary is the process: the socket, the tray,
  the signals, the directory it claims, all gated to the platforms that have
  them.
  The split is not tidiness. A page has the first half and none of the
  second, but it can run a dedicated worker — and a worker holding a session
  and speaking this protocol down a port *is* a daemon by every definition
  that matters here: one session per user, in one place, and a front end that
  holds none. It is also the only way to keep the store: SQLite's persistent
  VFS on the web is OPFS through a synchronous access handle, which exists in
  a dedicated worker and nowhere else.
- **oxidezap-plugin-abi**: the wasm ABI — its constants and the widget-tree
  codec. No dependencies and `no_std`, because it is compiled into the daemon
  *and* into every plugin, including ones with no allocator.
- **oxidezap-plugin-host**: runs `.wasm` plugins inside the daemon. Discovery,
  the sandbox, and the host half of the ABI. One wasmi `Store` and one
  bounded queue per plugin, on an OS thread where there is one and on the
  page's own loop where there is not — `sched/` is that split and it is two
  files, because a wasm call is synchronous either way and the loop above it
  is written once. Where a plugin's approvals and its own settings are kept
  is the other split (`store/`): files in a private directory, or the
  origin's `localStorage`.
  What is loaded is a *generation* rather than the host itself, because
  everything in the daemon holds the host: a connection, the session bridge
  and the tab listener each keep an `Arc<Plugins>` for their own lifetime, so
  a reload that built a new one would leave every one of them routing presses
  into a set nobody is running. `Live` is the generation, one lock holds it,
  and `Plugins::reload` is the only thing that swaps it.
- **oxidezap-plugin**: the Rust SDK a plugin is written against. Not a
  dependency of anything here; it exists to be built for wasm32. What it adds
  over the raw imports is what the compiler can check: two mask types so a
  capability cannot be passed where a set of event kinds goes, a `Setup` whose
  methods vanish once used so declaring twice is a missing method rather than
  a refusal at load, a size carried on each field so a read does not pick one,
  a UI builder whose sections take closures so there is no `end` to forget,
  and `Event::which`, which narrows an event to a view naming only the fields
  its kind carries — the absence rule is right for the wire and wrong for a
  handler, where reading `TEXT` off a `UI_ACTION` answers an empty string that
  no compiler and no log will ever question. Every one of those views carries
  an `Other`/`Unknown` arm, because a `match` that could not compile against a
  kind the daemon learned later would make every addition a breaking change.
  All of it monomorphizes away — the example is still a few kilobytes. What is
  *not* free is `log!`: formatting without a heap still pulls `core::fmt` in,
  which is about 2.6 KiB, so the choice is per plugin and the doc comment says
  the number rather than leaving somebody to find it in a size diff. The
  `plugin!` macro emits the `#[panic_handler]` too — boilerplate every plugin
  copied, and one of the two ways a first build fails — with `panic = own` for
  anyone who wants their own. Its `testing` feature answers the imports from a
  table a test owns, which is the only way to run a handler without the
  daemon; `raw::Ptr` exists for it, because an address is an `i32` on wasm32
  and truncates to nothing anywhere else.
- **oxidezap-gui**: GPUI front end, binary `oxidezap`. Talks to the daemon and
  starts one if none is listening. Owns video decode, which writes straight
  into `gpui::RenderImage` and is not reusable off GPUI.
  The same crate builds for `wasm32-unknown-unknown`, where `main` becomes the
  module's start function and the differences live in `platform/` — one
  function the interface calls, two implementations behind it, no `cfg`
  anywhere above. A component never learns that browsers exist, for the same
  reason it never learns that small screens do.

A front end depends on ipc/core/audio and never on session: there is exactly
one WhatsApp session per user, and it lives in the daemon. On the web it
depends on the *daemon* as well, and that is the same rule rather than an
exception to it — a page has no process to reach one in, so it starts one in
its own address space through `daemon::embedded`. The session is still the
daemon's, the window still owns none of it, and the protocol between them is
the protocol a socket carries everywhere else.

Which is also what makes a second tab ordinary rather than a conflict. One tab
per origin holds the account — the claim decides which — and every other tab is
a front end onto it over `ipc::tab`, holding no session, no store and no media,
exactly as a desktop window holds none. Nothing in the interface knows the
difference, and the rule it looks like it breaks is the rule it is an instance
of.

`examples/` holds plugins, and is excluded from the workspace: they build for
`wasm32-unknown-unknown` and link imports only the daemon provides, so a
`cargo build` at the root would try to link them for the host. `template/` is
the one to copy — it asks for nothing that touches the account, so it runs the
moment it is dropped in the folder — and `autoreply/` is the same shape with
something in it.

`xtask/` is the repository's own tooling — the web build, the bundle checks,
and the `gh-pages` publisher — and it is excluded from the workspace for a
reason of its own rather than the plugins'. The Pages publish job holds
`contents: write` and checks out one directory; a workspace member would make
cargo resolve the whole graph, eight git dependencies among them, before it
could compile a binary that needs none of it. So it carries its own
`[workspace]`, takes no dependencies at all, and CI runs its tests against its
own manifest the way it runs the example plugins'. What lives there was shell
until it was not: a compare-and-swap against a branch, and a three-way "is
this still wanted" whose one wrong reading — an operational failure collapsing
into "stand down" — is a deployment that silently does not happen while the
job reports success. Both now have tests, including one that drives the whole
publish against a bare repository in a temporary directory. `curl` and `gzip`
are the two things it still shells out to, and deliberately: a TLS client and
a deflate implementation are the two dependencies that would cost this
directory the property the sparse checkout depends on.

`docs/plugin-abi.md` is the contract for anyone not using the SDK: the imports
with their signatures, the field table by kind, the UI encoding, the outcome
codes and every bound the host holds a plugin to. The SDK is a convenience
over exactly that and has no privileged access, which is the sentence that
makes a TinyGo plugin possible — so the document is load-bearing rather than
descriptive, and the module it prints is loaded by a test
(`the_minimal_module_in_the_abi_document_loads`) rather than copied into one:
a copy is what lets the version literal in the snippet drift past
`abi::VERSION` with nothing to notice.

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

