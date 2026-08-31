//! The daemon's side of the transport between two tabs.
//!
//! One tab in an origin holds the account — it won the lock in
//! `crate::claim` — and it is a daemon in every sense this codebase uses the
//! word: it owns the session, the store and the media, and it serves front
//! ends over a protocol. What is new is that the front ends it serves are no
//! longer only its own window. A second tab is a window with no session,
//! which is what a desktop front end has always been, and this is the
//! endpoint it connects to.
//!
//! `serve_client` is untouched. It is generic over `AsyncRead + AsyncWrite`,
//! so a connection here is one end of an in-process duplex with its lines
//! moved across a `BroadcastChannel` — the same shape as the WebSocket
//! bridge, which does exactly this over a socket.
//!
//! # What one connection costs, and what bounds it
//!
//! A duplex, two tasks and a channel, out of the same [`MAX_CLIENTS`] every
//! other transport draws on: the tasks and the buffers come out of this tab's
//! memory however a front end arrived. Beyond that the browser's own
//! structured clone is the per-frame cost, once per connection — which is why
//! each connection has a channel of its own rather than sharing the
//! rendezvous, where every frame would be delivered to every tab in the
//! origin.
//!
//! # How a connection ends
//!
//! Not by anything sent. A tab that is killed posts no goodbye and a
//! `BroadcastChannel` has no close event, so the follower holds a lock for as
//! long as it wants serving and this side waits on it — the browser releases
//! it when that tab goes, whatever took it away. See
//! [`oxidezap_ipc::tabs::liveness_lock_for`].

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use oxidezap_ipc::tabs::fields::{
    bytes as bytes_field, flag as bool_field, number as number_field, set, string as string_field,
};
use oxidezap_ipc::tabs::{self, Rendezvous};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;
use web_sys::{BroadcastChannel, MessageEvent};

use crate::server::MAX_CLIENTS;
use crate::session_bridge::Commands;
use crate::state::StateHub;

/// How much of a frame may sit in one connection's pipe before the writer
/// waits.
///
/// Sized as the embedded pipe is, and for the same reason: a history load is
/// written in one go, and both ends are scheduled cooperatively on this
/// tab's one agent, so a full pipe is a yield rather than a deadlock.
const PIPE: usize = 1 << 18;

/// The rendezvous, answered for as long as this is held.
///
/// Dropping it stops this tab answering asks and closes the channel. That is
/// the right behaviour and not merely tidy: what drops it is the session
/// going away — an account forgotten, a bridge that stopped — and a tab that
/// no longer has a session must stop offering one. The followers see their
/// connections end, ask again, and one of them takes the lock.
pub(crate) struct Serving {
    channel: BroadcastChannel,
    /// The handler behind the channel. A `Closure` dropped while the browser
    /// still holds a reference is a panic rather than a missed call, so it
    /// lives exactly as long as the channel does and is detached from it
    /// first.
    _answering: Closure<dyn FnMut(MessageEvent)>,
}

impl Drop for Serving {
    /// Stops answering, and takes the handler off before dropping it: a
    /// browser holding a reference to a freed callback is a crash rather than
    /// a missed event.
    fn drop(&mut self) {
        self.channel.set_onmessage(None);
        self.channel.close();
    }
}

/// Start answering the tabs in this origin that have no session.
///
/// Returns once the channel is open, which is before any tab has asked.
/// Announcing on the way up is what makes a takeover quiet: the followers of
/// a tab that just closed are each sitting on an unanswered ask, and this is
/// what tells them to ask again rather than wait out their timeout.
///
/// # Errors
///
/// The browser would not open the channel. Not fatal to the account — this
/// tab holds the session either way — so the caller logs it and goes on
/// serving its own window.
pub(crate) fn serve(
    hub: &Arc<StateHub>,
    plugins: &Arc<oxidezap_plugin_host::Plugins>,
    commands: &Commands,
) -> Result<Serving, String> {
    let channel = BroadcastChannel::new(tabs::RENDEZVOUS)
        .map_err(|e| format!("this browser would not open a channel between tabs: {e:?}"))?;

    // How many front ends this tab is serving besides its own window.
    //
    // The window is not counted, because it is not this transport's: it is a
    // pipe `embedded::start` made, and it is one connection whatever happens
    // here. What this bounds is the thing that can multiply — a tab in a
    // reload loop, or a page somebody scripted — and it is the same cap every
    // other transport draws on rather than a second one beside it.
    let served = Rc::new(std::cell::Cell::new(0_usize));

    // The asks already being served, so that a repeat is not a second
    // connection.
    //
    // A follower re-sends its ask when it hears a tab announce that it holds
    // the account, because the ordinary race is an ask that arrived while
    // there was nobody listening. But the *other* order happens too — the ask
    // lands just after this handler is installed and just before the
    // announcement goes out — and then the same nonce is asked twice. Serving
    // it twice puts two `serve_client` instances on one channel: both read
    // every request, so one press sends one message twice, and their frames
    // interleave into a front end that has no way to tell them apart. An ask
    // is a connection's name, so the name is what is remembered.
    let serving: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));

    let hub = Arc::clone(hub);
    let plugins = Arc::clone(plugins);
    let commands = commands.clone();
    let rendezvous = channel.clone();
    let answering = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        let Some(line) = event.data().as_string() else {
            return;
        };
        let Some(Rendezvous::Ask { ask, .. }) = Rendezvous::decode(&line) else {
            return;
        };
        if served.get() >= MAX_CLIENTS {
            log::warn!("refusing a tab: already serving {MAX_CLIENTS} front ends");
            return;
        }
        // Answered again rather than ignored: the repeat exists because the
        // asking tab is not sure its first ask was heard, and the connection
        // it is waiting on is the one already open under this name.
        if !serving.borrow_mut().insert(ask.clone()) {
            log::info!("tab {ask} asked twice; answering the connection it already has");
            answer(&rendezvous, &ask);
            return;
        }
        log::info!("serving tab {ask}");
        served.set(served.get() + 1);
        accept(
            &rendezvous,
            &ask,
            Arc::clone(&hub),
            Arc::clone(&plugins),
            commands.clone(),
            Rc::clone(&served),
            Rc::clone(&serving),
        );
    });
    channel.set_onmessage(Some(answering.as_ref().unchecked_ref()));

    // Said after the handler is installed, not before: a follower answers
    // this by asking again, and an ask that arrives before there is anything
    // listening is one nobody hears.
    if let Some(line) = (Rendezvous::Leading { v: tabs::VERSION }).encode()
        && let Err(e) = channel.post_message(&wasm_bindgen::JsValue::from_str(&line))
    {
        log::debug!("this tab could not announce that it holds the account: {e:?}");
    }

    log::info!("serving other tabs of this origin");
    Ok(Serving {
        channel,
        _answering: answering,
    })
}

/// Serve one tab.
fn accept(
    rendezvous: &BroadcastChannel,
    ask: &str,
    hub: Arc<StateHub>,
    plugins: Arc<oxidezap_plugin_host::Plugins>,
    commands: Commands,
    served: Rc<std::cell::Cell<usize>>,
    serving: Rc<RefCell<HashSet<String>>>,
) {
    let name = tabs::channel_for(ask);
    let frames = match BroadcastChannel::new(&name) {
        Ok(frames) => frames,
        Err(e) => {
            log::error!("this browser would not open a channel for a tab: {e:?}");
            served.set(served.get().saturating_sub(1));
            serving.borrow_mut().remove(ask);
            return;
        }
    };

    // What this connection has staged and nobody has taken.
    //
    // A payload under `u-` is a front end's only copy of something it is
    // about to send, so the media sweep spares it — which means a tab that
    // records a note and then vanishes before sending or discarding it leaves
    // bytes nothing will ever reclaim, in a heap that has a ceiling. The
    // connection's own teardown is the one moment that knows those bytes are
    // now unreachable: the tab that named them is gone.
    let staged: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));

    let (client, server) = tokio::io::duplex(PIPE);
    oxidezap_session::spawn(async move {
        if let Err(e) = crate::server::serve_client(server, hub, plugins, commands).await {
            log::debug!("a tab disconnected: {e}");
        }
    });
    let (from_server, mut to_server) = tokio::io::split(client);

    // The write half as a queue into the task that owns it, exactly as the
    // front end's own pipe does it: writing to a duplex is an await, and the
    // browser hands this side its messages in a callback that cannot wait.
    let (requests, mut to_write) = tokio::sync::mpsc::unbounded_channel::<String>();
    oxidezap_session::spawn(async move {
        while let Some(line) = to_write.recv().await {
            if to_server.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
        // The client end, dropped. `serve_client` reads EOF and returns,
        // which is how a connection this side gave up on is torn down at the
        // other end of the pipe too.
    });

    // Everything the browser must not collect while the connection is open,
    // and the one place that lets go of it. The liveness watch below is what
    // clears it; nothing else here holds a strong reference.
    //
    // Installed, and that is the whole of what was missing: this handler was
    // built and held and never put on the channel, so the tab this side had
    // just agreed to serve wrote its hello into a channel nobody was
    // listening on. `serve_client` waited out its handshake window and
    // refused the connection, which is the one symptom that reached a
    // console — from the *asking* tab, naming a frame it had sent perfectly
    // well. Nothing about it pointed here.
    let handling = handler(&frames, &requests, &staged);
    frames.set_onmessage(Some(handling.as_ref().unchecked_ref()));
    let open = Rc::new(RefCell::new(Some(Connection {
        channel: frames.clone(),
        _handler: handling,
    })));

    // Reading the pipe and posting what comes out. Ends when `serve_client`
    // does — an error frame it refused the client with, a `Shutdown`, or the
    // teardown above.
    {
        let frames = frames.clone();
        let open = Rc::clone(&open);
        wasm_bindgen_futures::spawn_local(async move {
            let mut lines = BufReader::new(from_server).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if post_line(&frames, &line).is_err() {
                    break;
                }
            }
            // Said rather than merely stopped. The other tab is watching a
            // channel that will simply go quiet otherwise, and a front end
            // that never learns its connection ended never retries.
            let _ = post_bye(
                &frames,
                "the tab holding this account closed the connection",
            );
            open.borrow_mut().take();
        });
    }

    // The follower holds this for as long as it wants serving. Granted means
    // it has gone — closed, crashed, or navigated away — and there is nothing
    // to say to it.
    let live = tabs::liveness_lock_for(ask);
    let live_name = ask.to_string();
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = oxidezap_ipc::web_locks::wait_for(&live).await {
            // Nothing to fall back on, and better said than silently leaked:
            // without this the connection is held until the session goes.
            log::warn!("this tab cannot tell when a front end leaves: {e}");
            return;
        }
        open.borrow_mut().take();
        served.set(served.get().saturating_sub(1));
        // Forgotten with the connection, which is what keeps the set the size
        // of the tabs being served rather than of every tab ever served. The
        // repeat this guards against belongs to a connection that is opening;
        // once one has ended, its name is nobody's.
        serving.borrow_mut().remove(&live_name);
        // And so are the payloads it staged and never sent. A key the send
        // did reach is already gone — the session takes it when it reads it —
        // so this removes what is left, which is what nobody is coming back
        // for.
        //
        // The one thing it can race is a send whose command is still queued
        // behind others when the tab vanishes in the same instant. That send
        // then fails, in a window nobody is watching, which is the better half
        // of the trade against bytes that are never reclaimed at all.
        for key in std::mem::take(&mut *staged.borrow_mut()) {
            if crate::media::take(&key).is_some() {
                log::debug!("reclaimed {key}, staged by a tab that has gone");
            }
        }
        log::debug!("a tab stopped listening");
    });

    answer(rendezvous, ask);
}

/// Tell a tab which channel its connection is on.
///
/// On the channel this tab is already listening on, rather than a second
/// object opened to say one thing: a `BroadcastChannel` does not deliver to
/// the object that posted, so answering here is also what keeps this tab from
/// hearing its own answer and treating it as traffic.
fn answer(rendezvous: &BroadcastChannel, ask: &str) {
    if let Some(line) = (Rendezvous::Serve {
        v: tabs::VERSION,
        ask: ask.to_string(),
        on: tabs::channel_for(ask),
    })
    .encode()
        && let Err(e) = rendezvous.post_message(&wasm_bindgen::JsValue::from_str(&line))
    {
        log::error!("this tab could not answer another: {e:?}");
    }
}

/// One served connection's browser objects.
struct Connection {
    channel: BroadcastChannel,
    _handler: Closure<dyn FnMut(MessageEvent)>,
}

impl Drop for Connection {
    /// Closes this connection's channel, which is what "the front end has
    /// gone" amounts to on this side. The handler comes off first, for the
    /// reason above.
    fn drop(&mut self) {
        self.channel.set_onmessage(None);
        self.channel.close();
    }
}

/// What a follower says on its own channel, answered.
fn handler(
    frames: &BroadcastChannel,
    requests: &tokio::sync::mpsc::UnboundedSender<String>,
    staged: &Rc<RefCell<HashSet<String>>>,
) -> Closure<dyn FnMut(MessageEvent)> {
    let frames = frames.clone();
    let requests = requests.clone();
    let staged = Rc::clone(staged);
    Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        let data = event.data();
        let Some(kind) = string_field(&data, "k") else {
            return;
        };
        match kind.as_str() {
            "line" => {
                let Some(mut line) = string_field(&data, "s") else {
                    return;
                };
                // The terminator this transport does not carry and the pipe
                // needs: a channel frames its own messages, a byte stream
                // does not.
                line.push('\n');
                let _ = requests.send(line);
            }
            "read" => {
                let (Some(id), Some(key)) = (number_field(&data, "id"), string_field(&data, "key"))
                else {
                    return;
                };
                // `deliver` releases the claim a requested download holds
                // against the sweep, and `read` does not — the same two
                // answers the tab that owns the cache gives itself, asked
                // from one connection away.
                let bytes = if bool_field(&data, "once") {
                    crate::media::deliver(&key)
                } else {
                    crate::media::read(&key)
                };
                // The ceiling is checked here, before the copy, because here
                // is the only place it can be: what crosses is a
                // `Uint8Array` this tab builds and the browser then clones
                // into the asking tab, so a payload larger than that tab's
                // whole allowance is already spent by the time it could
                // refuse it. `most` absent means no ceiling — the answer to a
                // download somebody asked for is not rationed, and neither is
                // an older tab that does not know to send one.
                let most = number_field(&data, "most").unwrap_or(u64::MAX);
                let _ = match bytes {
                    Some(bytes) if bytes.len() as u64 > most => post_failure(
                        &frames,
                        "media",
                        id,
                        &format!("media {key} is larger than the asking tab's budget"),
                    ),
                    Some(bytes) => post_media(&frames, id, &bytes),
                    None => post_failure(&frames, "media", id, &format!("media {key} is not here")),
                };
            }
            "stage" => {
                let (Some(id), Some(key), Some(bytes)) = (
                    number_field(&data, "id"),
                    string_field(&data, "key"),
                    bytes_field(&data, "b"),
                ) else {
                    return;
                };
                let _ = match crate::media::put_owned(&key, bytes) {
                    Ok(_) => {
                        staged.borrow_mut().insert(key);
                        post_staged(&frames, id)
                    }
                    Err(e) => post_failure(&frames, "staged", id, &e.to_string()),
                };
            }
            "discard" => {
                let Some(key) = string_field(&data, "key") else {
                    return;
                };
                staged.borrow_mut().remove(&key);
                let _ = crate::media::take(&key);
            }
            _ => {}
        }
    })
}

/// One frame, on its way to the tab this connection serves.
///
/// Stripped of the terminator the pipe carries: a channel frames its own
/// messages, so a newline here would arrive inside the message.
fn post_line(frames: &BroadcastChannel, line: &str) -> Result<(), wasm_bindgen::JsValue> {
    let message = js_sys::Object::new();
    set(&message, "k", &"line".into())?;
    set(&message, "s", &line.into())?;
    frames.post_message(&message)
}

/// Say that this connection is over.
///
/// Said rather than merely stopped: the other tab is watching a channel that
/// would otherwise just go quiet, and a front end that never learns its
/// connection ended never retries.
fn post_bye(frames: &BroadcastChannel, why: &str) -> Result<(), wasm_bindgen::JsValue> {
    let message = js_sys::Object::new();
    set(&message, "k", &"bye".into())?;
    set(&message, "e", &why.into())?;
    frames.post_message(&message)
}

/// Hand over a payload, as bytes the browser clones rather than text.
fn post_media(
    frames: &BroadcastChannel,
    id: u64,
    bytes: &[u8],
) -> Result<(), wasm_bindgen::JsValue> {
    let message = js_sys::Object::new();
    set(&message, "k", &"media".into())?;
    set(&message, "id", &(id as f64).into())?;
    set(&message, "b", &js_sys::Uint8Array::from(bytes).into())?;
    frames.post_message(&message)
}

/// Confirm that a payload has landed in this tab's cache.
///
/// What the asking tab waits for before sending the request that names the
/// key: a frame that overtakes its own upload names a payload that is not
/// there.
fn post_staged(frames: &BroadcastChannel, id: u64) -> Result<(), wasm_bindgen::JsValue> {
    let message = js_sys::Object::new();
    set(&message, "k", &"staged".into())?;
    set(&message, "id", &(id as f64).into())?;
    frames.post_message(&message)
}

/// Answer a sideband request with why it could not be met.
///
/// Answered rather than left silent, because the asking side is waiting on a
/// deadline: an unanswered read costs it the whole allowance to learn what
/// one message could have told it at once.
fn post_failure(
    frames: &BroadcastChannel,
    kind: &str,
    id: u64,
    why: &str,
) -> Result<(), wasm_bindgen::JsValue> {
    let message = js_sys::Object::new();
    set(&message, "k", &kind.into())?;
    set(&message, "id", &(id as f64).into())?;
    set(&message, "e", &why.into())?;
    frames.post_message(&message)
}

/// Both ends of this transport, in a browser.
///
/// The parent module is `web_sys` against a real `BroadcastChannel` and a real
/// `LockManager`, so `cargo test` on the host does not compile a line of it —
/// and the cost of that gap was not theoretical. [`accept`] built its
/// connection handler, held it, and never put it on the channel. Every check
/// in the repository passed: it compiles, it is `#[must_use]`-free, the type
/// is held for exactly as long as it should be, and the only thing wrong is a
/// call that is not there.
///
/// What it did in a browser was serve the rendezvous perfectly — a second tab
/// asked, was answered, and reported itself attached — and then hear nothing
/// the tab said. `serve_client` waited out its handshake window and refused
/// the connection, so the only error anywhere appeared in the *asking* tab's
/// console, naming a hello it had sent correctly. Three rounds of review and
/// two rounds of automated review read this file without seeing it, because
/// reading is not what catches a missing call; running it is.
///
/// ```bash
/// # Chromium and its driver are what the runner needs; `RUSTFLAGS` is reset
/// # for the reason `examples/` resets it — the root's wasm flags are the web
/// # *front end's*, and a shared memory here would need isolation headers
/// # this runner does not serve.
/// CHROMEDRIVER=$(which chromedriver) \
/// RUSTFLAGS='--cfg web_sys_unstable_apis' \
/// CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
///   cargo test -p oxidezap-daemon --lib --target wasm32-unknown-unknown
/// ```
///
/// One agent runs both ends here, which is exactly what a `BroadcastChannel`
/// allows: it does not deliver to the object that posted, and every other
/// object of that name hears it — including one in this same page. So the
/// leader and the follower are the real `serve` and the real
/// `oxidezap_ipc::tab::connect`, talking over the real transport, with
/// nothing standing in for either side.
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use oxidezap_ipc::tab::FromTab;
    use oxidezap_ipc::{ClientRequest, DaemonMessage, PROTOCOL_VERSION, Request};
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

    use super::serve;
    use crate::state::StateHub;

    wasm_bindgen_test_configure!(run_in_browser);

    /// A daemon with nothing in it, and something to answer its commands.
    ///
    /// The stand-in bridge is not decoration. A front end that says it has a
    /// window is asked for a keyframe the moment its handshake lands, and
    /// `serve_client` *awaits that answer* — so a test that only held the
    /// receiver open would hang exactly where a real daemon would have
    /// replied. It answers everything the same way; nothing here asks a
    /// second thing.
    fn a_daemon() -> (
        Arc<StateHub>,
        Arc<oxidezap_plugin_host::Plugins>,
        crate::session_bridge::Commands,
    ) {
        let (commands, mut asked) = tokio::sync::mpsc::channel::<
            crate::session_bridge::SessionCommand,
        >(crate::server::MAX_CLIENTS);
        wasm_bindgen_futures::spawn_local(async move {
            while let Some(command) = asked.recv().await {
                let _ = command
                    .reply
                    .send(crate::session_bridge::CommandOutcome::Accepted);
            }
        });
        (
            StateHub::new(),
            Arc::new(oxidezap_plugin_host::Plugins::none(Arc::new(|_| {}))),
            commands,
        )
    }

    /// How long the whole exchange may take before it is a failure.
    ///
    /// Bounded because the failure this test exists for is a *silence*: the
    /// leader hearing nothing and answering nothing. Left unbounded, that
    /// reads as the runner giving up on a test it cannot see the end of,
    /// which says nothing about what went wrong — and it is the first thing
    /// this test did when its own stand-in bridge was missing.
    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(20);

    /// A tab that asks for the account is served, and what it says is heard.
    ///
    /// The regression test, and the assertion that matters is the second one.
    /// Being *answered* was never broken — the rendezvous handler has always
    /// been installed — so a test that stopped at "it attached" would have
    /// passed against the bug this exists for. What the bug ate was the first
    /// thing the attached tab said, and a snapshot coming back is the proof
    /// that the leader heard it: `serve_client` sends one only after it has
    /// read and accepted a hello.
    #[wasm_bindgen_test]
    async fn a_served_tab_is_heard_as_well_as_answered() {
        let (hub, plugins, commands) = a_daemon();
        let _serving = serve(&hub, &plugins, &commands).expect("the rendezvous opens");

        let mut tab = oxidezap_ipc::tab::connect()
            .await
            .expect("the tab holding the account answers");

        let hello = serde_json::to_vec(&Request::bare(ClientRequest::Hello {
            protocol: PROTOCOL_VERSION,
            session_events: true,
            has_window: true,
        }))
        .expect("a hello serializes");
        tab.link.send_line(&hello).expect("and goes out");

        let answer = oxidezap_session::with_timeout(tab.incoming.recv(), PATIENCE)
            .await
            .expect("the leader answers a hello it heard, well inside the handshake window");
        match answer {
            Some(FromTab::Line(line)) => {
                let frame: DaemonMessage =
                    serde_json::from_str(&line).expect("the daemon speaks this protocol");
                // `Hello` is the daemon's own first frame, carrying the
                // snapshot, and `serve_client` writes it only once it has read
                // and accepted the client's. Against the bug this exists for
                // the frame that arrives here instead is an `Error` — "no
                // hello within the handshake window" — which is the leader
                // saying it heard nothing at all.
                assert!(
                    matches!(frame, DaemonMessage::Hello { .. }),
                    "a hello that was heard is answered with a snapshot, not {frame:?}"
                );
            }
            // The other shape of the same failure: refused and closed before
            // this side was told anything it could parse.
            Some(FromTab::Closed(why)) => panic!("the connection ended instead: {why}"),
            None => panic!("the connection ended without a word"),
        }
    }
}
