//! WhatsApp client wrapper for UI integration

/// Voice calls, which are the one part of the session a page cannot run.
mod calls;

/// A stored row and a composed quote, read as the other side's shape.
mod convert;

/// Store rows read back as the chats a front end draws.
mod history;

/// Which events wait for which, and which never had to.
mod lanes;

/// What a message's media is, for both the roads it arrives on.
mod media;

/// Pages, their cursors, and where a read stops.
mod paging;

#[cfg(test)]
mod tests;

use calls::CallRegistry;
use convert::{account_event, quote_context};
use lanes::EventLanes;
use paging::{participant_keyed_chat, read_message_range};

/// How far back a read receipt reaches: the last row a front end was shown,
/// and the ids it is bounded by. Written in `paging`, beside the cursors.
pub use paging::ReadBoundary;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use log::{debug, error, info, warn};
use oxidezap_chat_store::ChatStore;
use tokio::sync::{Mutex, mpsc};
use whatsapp_rust::bot::Bot;
use whatsapp_rust::client::Client;
// The same type either way; only the road to it differs. On a desktop the
// library re-exports it, and in a browser that re-export is behind a default
// feature the wasm build drops — so it is named at its own crate there.
#[cfg(not(target_family = "wasm"))]
use whatsapp_rust::store::SqliteStore;
use whatsapp_rust::wacore::proto_helpers::MessageExt;
use whatsapp_rust::wacore::types::call::{CallAction, IncomingCall as WaIncomingCall};
use whatsapp_rust::wacore::types::events::{ChannelEventHandler, Event};
use whatsapp_rust::wacore::types::presence::{
    ChatPresence as WaChatPresence, ChatPresenceMedia, ReceiptType,
};
use whatsapp_rust::wacore_binary::jid::{Jid, JidExt, observe_str};
use whatsapp_rust::waproto::whatsapp as wa;
#[cfg(target_family = "wasm")]
use whatsapp_rust_sqlite_storage::SqliteStore;

use crate::exec::{Executor, Task};
use oxidezap_core::{
    Availability, CallVideoFrame, ChatMessage, ComposingKind, DownloadableMedia, IncomingCall,
    MessageStatus, SystemNotice, UiEvent, VideoStream, fallback_chat_name,
};

use crate::names::NameBook;
use crate::quoting::quoted_from;
use crate::video::{self, CameraLost, VideoPublisher, VideoSenderSlot};
use whatsapp_rust::wacore::download::MediaType as DownloadMediaType;

use crate::store::settings as store_settings;

/// Where the store lives on this platform. See [`crate::store`].
pub use crate::store::{database_path as resolve_database_path, prepare as prepare_store};

/// Delete the local session: device identity, Signal state and chat history
/// all live in the one SQLite file.
pub use crate::store::wipe as wipe_local_state;

/// User-facing copy for a server-ended session. The server sometimes supplies
/// its own text (account locks do); prefer it, and otherwise say plainly which
/// refusal arrived, because every one of these needs the same fix — pair again.
fn logout_message(event: &whatsapp_rust::types::events::LoggedOut) -> String {
    use whatsapp_rust::wacore::types::events::ConnectFailureReason;

    if let Some(msg) = &event.logout_message {
        // header and subtext are separate strings on the wire; join whichever
        // the server actually sent rather than picking one and dropping copy.
        let parts: Vec<&str> = [msg.header.as_deref(), msg.subtext.as_deref()]
            .into_iter()
            .flatten()
            .filter(|t| !t.is_empty())
            .collect();
        if !parts.is_empty() {
            return parts.join(" ");
        }
    }
    match event.reason {
        ConnectFailureReason::LoggedOut => "This device was unlinked from WhatsApp.".to_string(),
        ConnectFailureReason::AccountLocked => {
            "WhatsApp locked this account. Check the app on your phone.".to_string()
        }
        ConnectFailureReason::TempBanned => "WhatsApp temporarily banned this account.".to_string(),
        ConnectFailureReason::ClientOutdated => {
            "WhatsApp rejected this client as outdated.".to_string()
        }
        other => format!("WhatsApp ended the session ({other:?})."),
    }
}

/// Shared client handle for accessing the WhatsApp client from UI
pub type ClientHandle = Arc<Mutex<Option<Arc<Client>>>>;

/// Shared UI event sender for sending events from async operations
pub type UiEventSender = Arc<Mutex<Option<mpsc::UnboundedSender<UiEvent>>>>;

/// Shared chat-store handle (durable message history in the same SQLite file)
pub type ChatStoreHandle = Arc<Mutex<Option<Arc<ChatStore>>>>;

/// Shared handle on the session's one address book. See [`NameBook`].
type NameBookHandle = Arc<Mutex<Option<Arc<NameBook>>>>;

/// What the session's own task shares with the handle that started it.
///
/// One struct rather than eight parameters: every one of these is a handle
/// the caller keeps a copy of, and a list of eight is a list nobody can read.
struct Shared {
    client_handle: ClientHandle,
    calls: CallRegistry,
    chat_store_handle: ChatStoreHandle,
    names_handle: NameBookHandle,
    ui_sender: UiEventSender,
    shutdown: Arc<tokio::sync::Notify>,
    reload: Arc<tokio::sync::Notify>,
}

/// WhatsApp client wrapper that manages the connection and provides
/// a clean interface for UI operations.
pub struct WhatsAppClient {
    /// Where the session's work runs: a runtime on a thread of its own on a
    /// desktop, the page's event loop in a browser. See [`crate::exec`].
    exec: Executor,
    /// Shared client reference
    client_handle: ClientHandle,
    /// Shared UI event sender for sending events from operations like start_call
    ui_sender: UiEventSender,
    /// Live/ringing calls
    calls: CallRegistry,
    /// Where a call's video frames are published, once somebody has asked
    /// for them. Read per frame rather than captured, so a front end that
    /// resubscribes mid-call does not leave the pumps talking to a receiver
    /// that has gone.
    ///
    /// Its own channel rather than a `UiEvent`: an event is news that a
    /// reader which missed one has missed for good, and this is a stream
    /// whose newest frame is the only one worth having. It is also bounded,
    /// which the event channel is not — a camera that outran a stalled
    /// reader would otherwise grow the queue for as long as the call lasted.
    video_tx: VideoSenderSlot,
    /// Whether anybody is drawing what the cameras produce. See
    /// [`Self::set_video_publishing`].
    video_publishing: Arc<portable_atomic::AtomicBool>,
    /// Durable chat history (same SQLite file as the device store)
    chat_store: ChatStoreHandle,
    /// The session's address book, so a page served on request names people
    /// the way the load that produced the chat list did.
    names: NameBookHandle,
    /// Tears down `run_client` on retry: without it the replaced client's
    /// loop would keep the executor and the SQLite pool alive forever
    /// (bot.run() reconnects internally and never returns on its own).
    shutdown: Arc<tokio::sync::Notify>,
    /// Asks the history reloader for a full pass.
    ///
    /// The reloader is otherwise driven by store invalidations, which is right
    /// while a front end is attached and wrong the moment one attaches: it has
    /// no chats and nothing has changed, so nothing would arrive until the
    /// next message did.
    reload: Arc<tokio::sync::Notify>,
    /// Whether the client has been started
    started: bool,
}

impl WhatsAppClient {
    /// Create a new WhatsApp client wrapper. Errors when the executor cannot
    /// be built — a desktop builds a runtime, which resource exhaustion can
    /// refuse — so a retry can route to the error screen instead of panicking
    /// the thread that asked.
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            exec: Executor::new()?,
            client_handle: Arc::new(Mutex::new(None)),
            ui_sender: Arc::new(Mutex::new(None)),
            calls: CallRegistry::default(),
            video_tx: Arc::new(std::sync::Mutex::new(None)),
            // Closed until a window says otherwise: a daemon that starts with
            // nobody attached has nobody to publish to.
            video_publishing: Arc::new(portable_atomic::AtomicBool::new(false)),
            chat_store: Arc::new(Mutex::new(None)),
            names: Arc::new(Mutex::new(None)),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            reload: Arc::new(tokio::sync::Notify::new()),
            started: false,
        })
    }

    /// Stop the background run loop, so the executor and the SQLite handles
    /// drop with it. Idempotent; a signal fired before the loop is up still
    /// lands (notify_one stores a permit).
    pub fn shutdown(&self) {
        self.shutdown.notify_one();
    }

    /// Ask the session to stop, wait for it to finish closing, and let it go.
    ///
    /// The wait is what separates this from [`shutdown`](Self::shutdown): the
    /// session still has to disconnect the socket and close SQLite, and a
    /// caller that walks away without waiting can cut that short. Bounded, so
    /// a wedged session delays a teardown rather than preventing it.
    ///
    /// Takes the client, because letting go of it is part of the answer and
    /// is itself a platform question: on a desktop the executor owns a Tokio
    /// runtime, and tokio refuses to drop one inside an async context. That
    /// drop happens where the wait does, which on a desktop is a blocking
    /// thread and on a page is here.
    ///
    /// Returns whether it finished within `grace`. On the ordinary path that
    /// is only worth logging; on the "clear data and pair again" path it
    /// decides whether anything may be deleted at all.
    pub async fn close(mut self, grace: std::time::Duration) -> bool {
        self.shutdown();
        let finished = self.exec.join(grace).await;
        if !finished {
            warn!("session did not finish closing within {grace:?}");
        }
        let drained = self.drain_chat_store(grace).await;
        crate::exec::let_go(self).await;
        finished && drained
    }

    /// Commit whatever the chat store's writer is still holding.
    ///
    /// It is the one task the executor does not own. `ChatStore` spawns its
    /// writer itself — that queue is the store's own ordering guarantee, not
    /// the session's — so [`join`](crate::exec::Executor::join) above says
    /// nothing about it.
    ///
    /// On a desktop that is invisible, because letting go of the client drops
    /// the runtime the task was spawned on and the task goes with it. A page
    /// has no runtime to drop: the writer lives on the browser's event loop
    /// and outlives this call. And what follows this call on the one path
    /// that matters is deleting the database — so an account reset could
    /// unlink the store while the old account's writer was still draining
    /// into it, which is the partial wipe the delete-the-whole-file rule
    /// exists to prevent, arrived at from the other end.
    ///
    /// `ChatStore::close` is what is awaited rather than a flush, and the
    /// difference is the whole point: a flush says the queue is caught up, and
    /// the writer answers it and goes straight back to waiting with
    /// `SharedSqlite` still open. A close does not come back — it answers
    /// after the loop has broken and the handle is dropped — and an open
    /// handle is exactly what the deletion cannot run against here, because
    /// this store's browser VFS writes changed blocks *after* the commit and
    /// a page still held could land behind the delete and put the file back.
    /// Taking the handle is the other half: it drops the sender the session
    /// held, so nothing enqueues anything after.
    async fn drain_chat_store(&self, grace: std::time::Duration) -> bool {
        let Some(store) = self.chat_store.lock().await.take() else {
            return true;
        };
        match crate::exec::with_timeout(store.close(), grace).await {
            Some(Ok(())) => true,
            // Answered, and that is what is being asked. A batch that rolled
            // back is a write that never landed and a writer that panicked is
            // one that will never write again; neither is a hand still on the
            // database, so neither is a reason to refuse the wipe and leave
            // the person unable to pair again.
            Some(Err(e)) => {
                warn!("the chat store did not close cleanly: {e}");
                true
            }
            None => {
                warn!("the chat store was still writing after {grace:?}");
                false
            }
        }
    }

    /// Get the client handle for sending messages
    #[allow(dead_code)]
    pub fn client_handle(&self) -> ClientHandle {
        self.client_handle.clone()
    }

    /// Durable chat history store, once the client is up (None before init).
    #[allow(dead_code)]
    pub fn chat_store(&self) -> ChatStoreHandle {
        self.chat_store.clone()
    }

    /// Subscribe to the video of whatever call is up.
    ///
    /// Asked for rather than always produced: publishing costs a clone of
    /// every access unit, and a caller with nowhere to draw one (a tray, a
    /// notifier) should not pay for it. Calling this again replaces the
    /// previous subscriber, which is what a reconnecting front end wants.
    pub fn video_events(&mut self) -> mpsc::Receiver<CallVideoFrame> {
        let (tx, rx) = mpsc::channel(video::PUBLISH_DEPTH);
        *self.video_tx.lock().expect("video sender poisoned") = Some(tx);
        rx
    }

    /// Where a camera's finished frames go.
    ///
    /// Unused where there is no camera: the callers are the call methods, and
    /// a page's are refusals. The same is true of [`Self::camera_lost`] below.
    #[cfg_attr(target_family = "wasm", allow(dead_code))]
    fn video_publisher(&self) -> VideoPublisher {
        VideoPublisher {
            sender: Arc::clone(&self.video_tx),
            watched: Arc::clone(&self.video_publishing),
        }
    }

    /// Publish frames, or stop: the daemon says when anybody is drawing.
    ///
    /// A call runs whether or not a window is open — the peer is receiving
    /// our camera either way — so the pumps would otherwise go on copying
    /// every access unit out of the encoder's buffer and handing it to a
    /// daemon that discards it. The gate is read before the frame is built,
    /// so what it saves is the copy as well as the hop.
    ///
    /// Not the sender itself, which the daemon owns and must not lose: this
    /// is a door in front of it.
    pub fn set_video_publishing(&self, on: bool) {
        self.video_publishing
            .store(on, portable_atomic::Ordering::Relaxed);
    }

    /// What to do when a camera stops being a camera.
    ///
    /// Built per call rather than held, because it is a closure over the two
    /// things the teardown needs and a runtime to run it on — and because the
    /// only caller is the one opening a device.
    #[cfg_attr(target_family = "wasm", allow(dead_code))]
    fn camera_lost(&self) -> CameraLost {
        let calls = self.calls.clone();
        let ui_sender = self.ui_sender.clone();
        // A spawner rather than the executor: this is reported from whichever
        // thread noticed the device go, which is not one the executor knows
        // about, so there is no ambient one to find.
        let spawner = self.exec.spawner();
        Arc::new(move |call_id: String, camera_id| {
            let calls = calls.clone();
            let ui_sender = ui_sender.clone();
            spawner.spawn(async move {
                // The device is already gone; what is left is to stop
                // claiming otherwise — and to name the camera that died,
                // because this runs on a spawned task and a user who turned
                // video off and on again meanwhile must not have the
                // replacement torn down by its predecessor's failure.
                Self::stop_local_video(&calls, &ui_sender, &call_id, Some(camera_id)).await;
            });
        })
    }

    /// Start the WhatsApp client in a background thread
    ///
    /// Returns a receiver for UI events, or an error if already started
    pub fn start(&mut self) -> Result<mpsc::UnboundedReceiver<UiEvent>, &'static str> {
        if self.started {
            return Err("WhatsApp client already started");
        }
        self.started = true;

        let (ui_tx, ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let client_handle = self.client_handle.clone();
        let ui_sender = self.ui_sender.clone();
        let calls = self.calls.clone();
        let chat_store = self.chat_store.clone();
        let names = self.names.clone();
        let shutdown = self.shutdown.clone();
        let reload = self.reload.clone();

        let started = self.exec.start("oxidezap-session", async move {
            {
                let mut guard = ui_sender.lock().await;
                *guard = Some(ui_tx.clone());
            }
            Self::run_client(
                ui_tx,
                Shared {
                    client_handle,
                    calls,
                    chat_store_handle: chat_store,
                    names_handle: names,
                    ui_sender: ui_sender.clone(),
                    shutdown,
                    reload,
                },
            )
            .await;
        });
        if started.is_err() {
            self.started = false;
            return Err("failed to start the WhatsApp session");
        }

        Ok(ui_rx)
    }

    /// Internal async function to run the client
    async fn run_client(ui_tx: mpsc::UnboundedSender<UiEvent>, shared: Shared) {
        let Shared {
            client_handle,
            calls,
            chat_store_handle,
            names_handle,
            ui_sender,
            shutdown,
            reload,
        } = shared;
        // A cold start is the phases below, with an `.await` between each
        // pair — and on a page that is not the pause it looks like. A future
        // that is already ready resolves inside the microtask it was polled
        // in, and every await here is ready on that target: the asynchrony is
        // the desktop runtime's, and SQLite in a page is synchronous. So the
        // whole run lands as *one* block on the thread that draws. Measured on
        // the published page: 342ms with not one frame in it, between a window
        // that was animating before it and animating after.
        //
        // Hence a turn for the loop between phases, and a stopwatch on each.
        // Neither makes the work cheaper — the totals are the same totals, and
        // what would make them smaller is moving the session off the window's
        // agent altogether. What this does is stop one freeze being the shape
        // of it, and name where a page's first second goes at all: the module
        // is stripped, so a flame graph of it names nothing. A turn is an
        // *opportunity* to draw rather than a promise of one — the browser
        // decides whether a rendering pass fits between two tasks — which is
        // the honest claim and still the difference between five chances and
        // none.
        //
        // What a turn does not open is a teardown racing this. It lets the
        // bridge's loop run, so a `ForgetSession` can be accepted while the
        // store is still opening — but that was already true at `prepare`,
        // which really does await the browser, and what would make it
        // dangerous is ordered elsewhere: `close` raises the shutdown and
        // *joins the executor*, so a wipe waits for this future to return and
        // the forget path is gated on whether it did.
        let cold_start = wacore::time::Instant::now();
        // Whatever this platform has to do before a database can be opened
        // at all. Here rather than at the caller, because forgetting it is
        // not a failure anybody would see: a browser without its VFS opens a
        // database in memory quite happily and loses the account when the tab
        // closes.
        if let Err(e) = crate::store::prepare().await {
            error!("Failed to prepare the store: {e}");
            let _ = ui_tx.send(UiEvent::Error(format!("Database error: {e}")));
            return;
        }
        let prepared = cold_start.elapsed();
        crate::exec::breathe().await;
        // Device store + durable chat history share one SQLite file (one pool,
        // one WAL writer).
        let db_path = match crate::exec::unblock(resolve_database_path).await {
            Ok(path) => path,
            Err(e) => {
                error!("Failed to resolve database path: {e}");
                let _ = ui_tx.send(UiEvent::Error("Database initialization failed".to_string()));
                return;
            }
        };
        let opening = wacore::time::Instant::now();
        let backend = match SqliteStore::with_config(&db_path, store_settings()).await {
            Ok(store) => store,
            Err(e) => {
                // With the chain, not just the head. `StoreError`'s own
                // Display is a category ("database connection error") and
                // what went wrong is always in its source — which on a page
                // is the only thing there is to go on, since there is no
                // database file anybody can open afterwards and look at.
                error!("Failed to create SQLite backend: {}", because(&e));
                let _ = ui_tx.send(UiEvent::Error(format!("Database error: {}", e)));
                return;
            }
        };
        let opened = opening.elapsed();
        crate::exec::breathe().await;

        let materializing = wacore::time::Instant::now();
        let chat_store = match ChatStore::new(&backend).await {
            Ok(store) => store,
            Err(e) => {
                error!("Failed to open chat store: {}", because(&e));
                let _ = ui_tx.send(UiEvent::Error(format!("Database error: {}", e)));
                return;
            }
        };
        let materialized = materializing.elapsed();
        {
            let mut guard = chat_store_handle.lock().await;
            *guard = Some(chat_store.clone());
        }

        // One book for the whole session, so a live bubble, the row it lands
        // in and the typing line above it are all naming the same person from
        // the same answer.
        let names = Arc::new(NameBook::new(chat_store_handle.clone()));
        // Published for the paged reads, which run outside this task and have
        // to name people the same way it does.
        {
            let mut guard = names_handle.lock().await;
            *guard = Some(names.clone());
        }
        // After *both* handles, not between them. A yield is a place other
        // tasks run, and a paged read is one of them: it wants the store and
        // the book together, and the gap that publishing them either side of
        // a yield would open is the one thing this change must not add.
        crate::exec::breathe().await;

        // Transport, HTTP client and runtime come from whichever platform
        // this is: the library's default features on a desktop, `web-sys`
        // bindings in a page. See `crate::net`.
        //
        // The version is not among them, and used to be: `sw.js` is
        // unreachable from a page — no `Access-Control-Allow-Origin`, and its
        // `Sec-Fetch-Site` gate is a header a browser will not let anyone set
        // — so this side fetched the number from a CDN feed and announced it
        // with `with_version`. The library resolves it per target now, off
        // the Facebook JS SDK bundle where Meta publishes the same `www`
        // revision cross-origin, so the answer belongs there again. Ours was
        // worse than redundant: `with_version` is an override, and an
        // override makes the library skip its own resolution *and* the
        // day-long cache stamp behind it.
        let building = wacore::time::Instant::now();
        let builder = crate::net::with_platform_plugins(Bot::builder()).with_backend(backend);
        let bot = match builder.build().await {
            Ok(bot) => bot,
            Err(e) => {
                error!("Failed to build bot: {}", e);
                let _ = ui_tx.send(UiEvent::Error(format!("Connection failed: {}", e)));
                return;
            }
        };

        // Give the client this platform's way onto a call's media wire, before
        // anything can ring. Nothing on a desktop, where the library's own UDP
        // dialler is the default and is right; on a page it is the whole
        // reason a call can be placed at all. See `crate::relay`.
        let built = building.elapsed();
        crate::exec::breathe().await;

        crate::relay::install(&bot.client());

        // Hydrate the UI from durable history before the network is even up
        // (bot.run() is what connects). The client is needed here so hydrated
        // JIDs normalize through the same PN->LID mapping live events use.
        let hydrating = wacore::time::Instant::now();
        let mut hydrated = 0;
        match Self::load_history(&chat_store, &bot.client(), &names).await {
            Ok(loaded) if !loaded.chats.is_empty() => {
                hydrated = loaded.chats.len();
                let _ = ui_tx.send(loaded.into_event());
            }
            Ok(_) => {}
            Err(e) => warn!("Failed to load chat history: {e}"),
        }
        // One line rather than five, and at `info` because a cold start
        // happens once: this is the only place the page's first second is
        // attributable at all, and the numbers are what any change to it has
        // to be argued against.
        info!(
            "cold start: store {prepared:?}, sqlite {opened:?}, chats {materialized:?}, \
             client {built:?}, hydration {:?} ({hydrated} chats) — {:?} in total",
            hydrating.elapsed(),
            cold_start.elapsed(),
        );
        crate::exec::breathe().await;

        // The chat store materializes history straight off the event bus.
        // detach: the store materializes for the whole session, so the
        // subscription must outlive this scope rather than unregister on drop.
        bot.client()
            .subscribe_handler(chat_store.handler())
            .detach();

        // And so does the UI, through the same door rather than the builder's
        // `on_event`. The closure registrars want a `Send` future, which is a
        // bound a page cannot meet: the `Arc<Client>` a handler is handed is
        // not `Send` there, because the transport it holds is a
        // `web_sys::WebSocket`. `EventHandler` is the surface the library
        // already relaxed for this, and the chat store above was using it
        // before we were.
        //
        // Nothing is missed by subscribing after the build: `bot.run()` is
        // what connects, and this is the same window the store's own
        // subscription sits in. The channel is unbounded and delivery is a
        // `try_send`, so a slow reader cannot back up the dispatch path — and
        // one task per event keeps the concurrent delivery the builder was
        // giving us, rather than quietly turning the event stream serial.
        let (events, incoming) = ChannelEventHandler::new();
        // What tells this session's own tasks that it is over.
        //
        // A desktop session ends by dropping the runtime it was built on,
        // which takes every task on it. A page has no such runtime: a
        // `spawn_local` task is never cancelled by anything, so one that holds
        // an `Arc<ChatStore>` or a client handle keeps them alive for the life
        // of the tab — and "clear data and pair again" then deletes the
        // database out from under a connection somebody still has open.
        //
        // Both long-lived children below watch this. A `watch` rather than a
        // `Notify` because dropping the sender is itself the signal: whichever
        // way `run_client` returns, including a panic on the way out, the
        // children wake and stop. There is no notification for a task to have
        // been too late to register for.
        // Named, not `_`: a `_` binding drops at once, which would end the
        // session before it began. This one is never sent on and never read —
        // holding it until `run_client` returns is the whole of its job.
        let (_session_over, stopping) = tokio::sync::watch::channel(());

        bot.client().subscribe_handler(events).detach();
        {
            let client = bot.client();
            let ui_tx = ui_tx.clone();
            let calls = calls.clone();
            let ui_sender = ui_sender.clone();
            let names = names.clone();
            let mut stopping = stopping.clone();
            crate::exec::spawn_owned(async move {
                // The dispatch loop's own handle. The one below is moved into
                // the per-event closure, and what this asks the client is the
                // PN/LID pairing that decides the lane.
                let dispatch_client = client.clone();
                let mut lanes = EventLanes::new(
                    move |event| {
                        let client = client.clone();
                        let ui_tx = ui_tx.clone();
                        let calls = calls.clone();
                        let ui_sender = ui_sender.clone();
                        let names = names.clone();
                        async move {
                            Self::handle_event(event, client, ui_tx, calls, ui_sender, names).await;
                        }
                    },
                    stopping.clone(),
                );
                loop {
                    let event = tokio::select! {
                        event = incoming.recv() => match event {
                            Ok(event) => event,
                            Err(_) => break,
                        },
                        // The session has gone. Anything still queued belongs
                        // to an account this task no longer speaks for.
                        _ = stopping.changed() => break,
                    };
                    // The kind, and only the kind. It is `Copy`, carries no
                    // payload and names the variant, which makes this the one
                    // account of the event stream that can be pasted into an
                    // issue: a `Debug` of the event itself would be somebody's
                    // messages. Worth having at all because a session that
                    // goes quiet is otherwise indistinguishable from one with
                    // nothing to say — the arms below speak only for the
                    // variants they handle.
                    debug!("client event: {:?}", event.kind());
                    lanes.dispatch(&dispatch_client, event).await;
                }
            });
        }

        // Re-hydrate the UI off the store's invalidation stream instead of raw
        // client events: changes are emitted only after commit, so a reload can
        // never observe pre-commit state (no flush barrier, no dispatch-order
        // dependency), and the debounce coalesces history-sync bursts into one
        // reload. HistoryLoaded merges, so re-sending is safe.
        Self::spawn_history_reloader(
            chat_store.subscribe(),
            chat_store.clone(),
            &bot,
            &ui_tx,
            reload,
            names,
            stopping,
        );

        // Store client reference for UI to use
        {
            let mut guard = client_handle.lock().await;
            *guard = Some(bot.client());
        }

        // Notify UI that init is complete
        let _ = ui_tx.send(UiEvent::InitComplete);

        // bot.run() reconnects internally, so on its own it only returns after
        // a logout; the shutdown signal is how a replaced client's thread gets
        // to exit (letting block_on return drops the runtime + SQLite pool).
        let client = bot.client();
        tokio::select! {
            // Said out loud, because this is a session ending. `run`
            // reconnects internally, so it returns only when there is nothing
            // left to try — and returning quietly left a window sitting on
            // "Connecting to WhatsApp" with a console that had stopped saying
            // anything at all, which reads as a hang rather than as a stop.
            () = bot.run() => info!("the WhatsApp session ended"),
            _ = shutdown.notified() => {
                // Graceful stop: flushes state and closes the transport. The
                // dropped run future is not awaited out instead, because a
                // disconnect() landing before run()'s first poll would be
                // clobbered by run's own is_running swap.
                client.disconnect().await;
            }
        }
    }

    /// Handle events from the WhatsApp client
    async fn handle_event(
        event: Arc<Event>,
        client: Arc<Client>,
        ui_tx: mpsc::UnboundedSender<UiEvent>,
        calls: CallRegistry,
        ui_sender: UiEventSender,
        names: Arc<NameBook>,
    ) {
        match &*event {
            Event::PairingQrCode(qr) => {
                info!("QR code received");
                let _ = ui_tx.send(UiEvent::QrCode {
                    code: qr.code.clone(),
                    timeout_secs: qr.timeout.as_secs(),
                });
            }
            Event::PairingCode(pair) => {
                info!("Pair code received");
                let _ = ui_tx.send(UiEvent::PairCode {
                    code: pair.code.clone(),
                    timeout_secs: pair.timeout.as_secs(),
                });
            }
            Event::SelfPushNameUpdated(update) => {
                info!("Push name is now {:?}", update.new_name);
                let _ = ui_tx.send(account_event(&client));
            }
            Event::PairSuccess(_) => {
                info!("Pairing successful, syncing...");
                let _ = ui_tx.send(UiEvent::PairSuccess);
                let _ = ui_tx.send(account_event(&client));
            }
            Event::Connected(_) => {
                info!("Connected to WhatsApp!");
                let _ = ui_tx.send(UiEvent::Connected);
                // Who this device is linked as. Read from the device store
                // rather than remembered from pairing: a client attaching
                // after a restart never saw that, and the account row was
                // claiming "not linked" over a live session.
                let _ = ui_tx.send(account_event(&client));
            }
            Event::LoggedOut(logged_out) => {
                info!("Logged out from WhatsApp: {:?}", logged_out.reason);
                // Not a Disconnected: reconnecting reuses the credentials the
                // server just rejected, which is the 401 loop.
                let _ = ui_tx.send(UiEvent::LoggedOut(logout_message(logged_out)));
            }
            Event::IncomingCall(call) => match &call.action {
                CallAction::Offer {
                    call_id, is_video, ..
                } => {
                    if call.offline {
                        info!("Ignoring offline call {} (stale)", call_id);
                        return;
                    }
                    info!("Incoming call from {}", call.from.observe());
                    let offer = Arc::new(call.clone());
                    calls.offer(call_id.clone(), offer.clone());
                    let caller_jid = normalize_chat_jid(&client, &call.from.to_string()).await;
                    let caller_name = call
                        .notify
                        .as_deref()
                        .filter(|name| !name.trim().is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| fallback_chat_name(&call.from));
                    let ui_call = IncomingCall::new(
                        call_id.clone(),
                        caller_name,
                        caller_jid,
                        *is_video,
                        &offer,
                    );
                    let _ = ui_tx.send(UiEvent::IncomingCall(ui_call));
                }
                CallAction::Accept { call_id, .. } => {
                    info!("Call {} accepted by peer", call_id);
                    let _ = ui_tx.send(UiEvent::CallAccepted(call_id.clone()));
                    // And what our camera is doing, which nothing has been
                    // able to say until now: a call this side placed as video
                    // opened its camera while the call was still *ringing*,
                    // and a ringing call has no live state for a camera to be
                    // recorded against. This is the first moment it does —
                    // after the acceptance above, which is what creates it.
                    // The call has somewhere to be drawn, and needs a point
                    // to start decoding from. Until this moment it was
                    // ringing: no window had a live call to put either
                    // direction in, so nothing was published and the next
                    // unit alone references frames no decoder starting now
                    // has ever seen.
                    if calls.camera_became_drawable(call_id) {
                        let _ = ui_tx.send(UiEvent::CallVideoChanged {
                            call_id: call_id.clone(),
                            stream: VideoStream::Local,
                            on: true,
                        });
                    }
                }
                CallAction::Reject { call_id, .. } => {
                    info!("Call {} rejected by peer", call_id);
                    calls.ended_remotely(call_id);
                    // Claimed, not assumed. The watcher parked on this call's
                    // `wait_ended` publishes the same event when the hangup
                    // above resolves, and either of the two can get there
                    // first; whichever does is the one that says it.
                    if calls.announce_ending(call_id) {
                        let _ = ui_tx.send(UiEvent::CallEnded(call_id.clone()));
                    }
                }
                CallAction::Terminate { call_id, .. } => {
                    info!("Call {} terminated by peer", call_id);
                    calls.ended_remotely(call_id);
                    // Claimed, not assumed. The watcher parked on this call's
                    // `wait_ended` publishes the same event when the hangup
                    // above resolves, and either of the two can get there
                    // first; whichever does is the one that says it.
                    if calls.announce_ending(call_id) {
                        let _ = ui_tx.send(UiEvent::CallEnded(call_id.clone()));
                    }
                }
                _ => {}
            },
            Event::MissedCall(missed) => {
                info!(
                    "Missed call {} from {}",
                    missed.call_id,
                    missed.from.observe()
                );
                calls.ended_remotely(&missed.call_id);
                // See the terminate arm.
                if calls.announce_ending(&missed.call_id) {
                    let _ = ui_tx.send(UiEvent::CallEnded(missed.call_id.clone()));
                }
            }
            Event::CallEndedElsewhere(ended) => {
                info!("Call {} handled on another device", ended.call_id);
                // Unconditional, unlike the arms above: this is a different
                // sentence from the watcher's `CallEnded` — it says *where* the
                // call went — so it is not the same news said twice.
                calls.ended_remotely(&ended.call_id);
                let _ = ui_tx.send(UiEvent::CallEndedElsewhere(ended.call_id.clone()));
            }
            Event::Messages(batch) => {
                // A drain is a backlog, not an arrival: fetching every
                // picture in it before the first bubble reaches the window
                // spends the whole reconnection on work the store has
                // already materialized and hydration would redo from the
                // thumbnail anyway.
                let eager = matches!(
                    batch.origin,
                    whatsapp_rust::wacore::types::events::BatchOrigin::Live
                );
                for inbound in batch.iter() {
                    Self::handle_inbound_message(
                        &inbound.message,
                        &inbound.info,
                        &client,
                        &ui_tx,
                        &names,
                        eager,
                    )
                    .await;
                }
            }
            Event::Receipt(receipt) => {
                // Delivered used to be dropped here, which is why the
                // second tick never appeared: only Read and Played reached
                // the UI, so a message went from one tick straight to blue.
                let Some(dominated_type) = (match &receipt.r#type {
                    ReceiptType::Delivered => Some(ReceiptType::Delivered),
                    ReceiptType::Read | ReceiptType::ReadSelf => Some(ReceiptType::Read),
                    ReceiptType::Played | ReceiptType::PlayedSelf => Some(ReceiptType::Played),
                    _ => None,
                }) else {
                    return;
                };

                debug!(
                    "Receipt {:?} for {} message(s) in {}",
                    dominated_type,
                    receipt.message_ids.len(),
                    receipt.source.chat.observe()
                );

                // Normalize the chat JID
                let normalized_chat_jid =
                    normalize_chat_jid(&client, &receipt.source.chat.to_string()).await;

                let _ = ui_tx.send(UiEvent::ReceiptReceived {
                    chat_jid: normalized_chat_jid,
                    message_ids: receipt.message_ids.clone(),
                    receipt_type: dominated_type,
                });
            }
            Event::ChatPresence(update) => {
                // Our own composing state, echoed back from another of our
                // devices, is not somebody typing *at* us.
                if update.source.is_from_me {
                    return;
                }
                let composing = match update.state {
                    WaChatPresence::Composing => Some(match update.media {
                        ChatPresenceMedia::Audio => ComposingKind::Audio,
                        ChatPresenceMedia::Text => ComposingKind::Text,
                    }),
                    WaChatPresence::Paused => None,
                };
                let chat_jid = normalize_chat_jid(&client, &update.source.chat.to_string()).await;
                let sender = update.source.sender.clone();
                // Keyed by the same JID whichever alias the event arrived
                // under. The registry is a map, and `clear_composing` looks
                // the entry up by this key: a composing under the PN with the
                // paused under the LID left one nobody could find, so the line
                // said they were typing until the TTL ran out — and alternating
                // events listed the same person twice.
                let identity = names.identity(&client, &sender).await;
                // Named here rather than left to the front end: the typing
                // line sits directly under this person's bubbles, and a name
                // picked by a different rule is the same person twice.
                let sender_name = match composing {
                    Some(_) => names.known(&client, &sender, None).await,
                    // Nobody draws the name of someone who stopped.
                    None => None,
                };
                let _ = ui_tx.send(UiEvent::ChatPresence {
                    chat_jid,
                    sender_jid: identity.canonical_jid.clone(),
                    sender_name,
                    composing,
                });
            }
            Event::Presence(update) => {
                let availability = if update.unavailable {
                    update
                        .last_seen
                        .map_or(Availability::Unknown, Availability::LastSeen)
                } else {
                    Availability::Online
                };
                // Normalized like the receipt and chat-presence branches
                // beside it: a chat whose JID was migrated from a phone
                // number to a LID is keyed by the LID, so presence published
                // under the PN alias would never reach it.
                let jid = normalize_chat_jid(&client, &update.from.to_string()).await;
                let _ = ui_tx.send(UiEvent::PresenceUpdated { jid, availability });
            }
            // Something happened *to* the group. Only the changes a member
            // would notice become a row; the rest is bookkeeping, and a line
            // for each would bury the conversation.
            Event::GroupUpdate(update) => {
                // A notice is a sentence about people who also have bubbles
                // and a typing line on the same screen. Naming them from the
                // stanza alone gave one person two names on one conversation:
                // the push name here, the address-book name everywhere else.
                let mut named = crate::group_notice::ResolvedNames::new();
                let mentioned = update
                    .participant
                    .iter()
                    .cloned()
                    .chain(
                        crate::group_notice::participants_of(&update.action)
                            .iter()
                            .map(|participant| participant.jid.clone()),
                    )
                    .collect::<Vec<_>>();
                for jid in mentioned {
                    let key = jid.to_string();
                    if named.contains_key(&key) {
                        continue;
                    }
                    // Resolved, or the same last resort every other surface
                    // falls back to: a notice naming somebody by the digits of
                    // a LID reads as a phone number that is not one.
                    let name = match names.known(&client, &jid, None).await {
                        Some(name) => name,
                        None => names.identity(&client, &jid).await.fallback_name.clone(),
                    };
                    named.insert(key, name);
                }
                let actor = crate::group_notice::actor_name(
                    update.participant.as_ref(),
                    update.participant_username.as_deref(),
                    &named,
                );
                if let Some(text) = crate::group_notice::describe(
                    &update.action,
                    actor.as_deref(),
                    update.participant.as_ref(),
                    &named,
                ) {
                    let _ = ui_tx.send(UiEvent::SystemNotice {
                        chat_jid: update.group_jid.to_string(),
                        // The stanza id plus the index within it: one
                        // notification can carry several actions, and a
                        // redelivery must not stack a second copy of any.
                        notice_id: format!(
                            "group-{}-{}",
                            update
                                .notification_id
                                .clone()
                                .unwrap_or_else(|| update.timestamp.timestamp_millis().to_string()),
                            update.action_index
                        ),
                        at: update.timestamp,
                        notice: SystemNotice::GroupChanged(text),
                    });
                }
            }
            _ => {
                let _ = ui_sender; // silences unused when no branch needs it
            }
        }
    }

    /// One decrypted inbound message -> UiEvent (reaction or chat message).
    async fn handle_inbound_message(
        msg: &wa::Message,
        info: &whatsapp_rust::wacore::types::message::MessageInfo,
        client: &Arc<Client>,
        ui_tx: &mpsc::UnboundedSender<UiEvent>,
        names: &NameBook,
        eager: bool,
    ) {
        // Use MessageExt to unwrap ephemeral/device_sent/view_once wrappers
        let base_msg = msg.get_base_message();

        // Check if this is a reaction message
        if let Some(reaction) = base_msg.reaction_message.as_option() {
            if let Some(key) = reaction.key.as_option()
                && let Some(target_id) = &key.id
            {
                let emoji = reaction.text.clone().unwrap_or_default();
                debug!(
                    "Reaction '{}' from {} on message {}",
                    emoji,
                    info.source.sender.observe(),
                    target_id
                );

                // Use remote_jid from key if available, otherwise use chat from info
                let chat_jid = key
                    .remote_jid
                    .clone()
                    .unwrap_or_else(|| info.source.chat.to_string());

                let normalized_chat_jid = normalize_chat_jid(client, &chat_jid).await;

                // One person, one reaction. `ChatMessage::add_reaction` keys
                // by this string and enforces one per sender — and a removal
                // is an *empty* reaction matched the same way — so a react
                // under the phone number and its replacement or removal under
                // the LID were two different people: the first emoji stayed up
                // and the second was counted beside it.
                let reactor = names.identity(client, &info.source.sender).await;

                let _ = ui_tx.send(UiEvent::ReactionReceived {
                    chat_jid: normalized_chat_jid,
                    message_id: target_id.clone(),
                    sender: reactor.canonical_jid.clone(),
                    emoji,
                });
            }
            return;
        }

        // Revokes/edits and other protocol stubs carry no displayable body;
        // the chat store materializes them durably (a reload shows the right
        // state), so don't fabricate a "[Media]" bubble under their own id.
        if base_msg.protocol_message.is_set() {
            return;
        }

        // The same question the store asks, so a live conversation and a
        // reloaded one agree. A poll update, an encrypted reaction or comment,
        // a pin or a keep-in-chat carries nothing to draw: published live it
        // is a `[Media]` bubble with an unread badge, and the store writes no
        // row for it, so it vanishes at the next hydration having already
        // raised a count nothing sent a receipt for.
        if oxidezap_chat_store::is_control_only(msg) {
            return;
        }

        // Try to extract media content
        let media_result = media::media_now(base_msg, client, eager).await;

        // Extract text content
        let content = msg
            .text_content()
            .map(|s| s.to_string())
            .or_else(|| msg.get_caption().map(|s| s.to_string()))
            .unwrap_or_else(|| {
                if media_result.is_some() {
                    String::new() // Empty for media-only messages
                } else {
                    "[Media]".to_string()
                }
            });

        let mut chat_message = ChatMessage {
            id: info.id.clone(),
            sender: info.source.sender.to_string(),
            sender_name: None, // Will be set in handle_message_received for groups
            content,
            timestamp: info.timestamp,
            is_from_me: info.source.is_from_me,
            is_read: false,
            media: None,
            reactions: std::collections::HashMap::new(),
            // Our own message echoed back has by definition reached the
            // server; someone else's carries no send state of ours.
            status: if info.source.is_from_me {
                MessageStatus::Sent
            } else {
                MessageStatus::default()
            },
            quoted: quoted_from(base_msg),
            revoked: false,
            system: None,
        };

        if let Some(media) = media_result {
            chat_message.media = Some(media);
        }

        Self::canonicalize_quoted_authors(client, names, std::slice::from_mut(&mut chat_message))
            .await;

        // Normalize chat JID to LID if mapping exists, so the same user doesn't
        // appear as two chats when messages come from PN vs LID.
        let normalized_chat_jid = normalize_chat_jid(client, &info.source.chat.to_string()).await;

        // One person, one row in the status feed. The broadcast is grouped by
        // sender, and the same contact reaches it under a phone number on
        // some updates and their LID on others — which splits their ring,
        // their unseen count and their playback run in two until a reload.
        // Hydration canonicalizes these (`hydrate_sender_names`); the live
        // path shipped whatever the envelope said, so the split came back
        // with every update that arrived under the other alias.
        if info.source.chat.is_status_broadcast() {
            chat_message.sender = names
                .identity(client, &info.source.sender)
                .await
                .canonical_jid
                .clone();
        }

        // The push name is what the sender calls themselves; the address
        // book is what this account's owner calls them, and that is the one
        // the phone shows. Resolving here rather than shipping the raw push
        // name is what stops one person appearing as two — the reloaded
        // bubble under the name you saved, the live one under theirs.
        let sender_name = if info.source.is_from_me {
            None
        } else {
            names
                .known(
                    client,
                    &info.source.sender,
                    Some(info.push_name.as_str()).filter(|name| !name.is_empty()),
                )
                .await
        };

        let _ = ui_tx.send(UiEvent::MessageReceived {
            chat_jid: normalized_chat_jid,
            message: Box::new(chat_message),
            sender_name,
        });
    }

    /// Send a text message to a chat.
    ///
    /// The returned handle completes when the send has run to its conclusion,
    /// successful or not. A caller that only fires and forgets can drop it;
    /// one that has to bound how much work it has outstanding (the daemon,
    /// driven by a program rather than by a person clicking) needs to know
    /// when the work it asked for is over.
    pub fn send_message(
        &self,
        jid_str: &str,
        content: &str,
        local_id: String,
        quoted: Option<oxidezap_core::QuotedMessage>,
    ) -> Task<()> {
        let client_handle = self.client_handle.clone();
        let chat_store = self.chat_store.clone();
        let ui_sender = self.ui_sender.clone();
        let jid_str = jid_str.to_string();
        let content = content.to_string();

        self.exec.spawn(async move {
            let jid: Jid = match jid_str.parse() {
                Ok(j) => j,
                Err(e) => {
                    error!("Invalid JID {}: {}", observe_str(&jid_str), e);
                    // The optimistic bubble still carries its local id;
                    // without this it would sit unsent with no indicator.
                    notify_send_failed(&ui_sender, &jid_str, &local_id, e.to_string()).await;
                    return;
                }
            };

            // Clone the Arc and release the mutex: a slow network call
            // here must not queue every other client action behind it.
            let client = client_handle.lock().await.clone();
            if let Some(client) = client {
                // A reply is the same send with a quote attached, which the
                // wire carries as an extended text message: a bare
                // `conversation` has nowhere to put the context.
                let message = match &quoted {
                    Some(quoted) => wa::Message {
                        extended_text_message: whatsapp_rust::buffa::MessageField::some(
                            wa::message::ExtendedTextMessage {
                                text: Some(content.clone()),
                                context_info: whatsapp_rust::buffa::MessageField::some(
                                    quote_context(quoted),
                                ),
                                ..Default::default()
                            },
                        ),
                        ..Default::default()
                    },
                    None => wa::Message {
                        conversation: Some(content.clone()),
                        ..Default::default()
                    },
                };

                // Record BEFORE sending: the server ack event fires during
                // send_message, so a row recorded after it would stay
                // Pending forever (the ack precedes it in writer order).
                let msg_id = client.generate_message_id();
                // Receipts/reactions arrive keyed by this id; rename the
                // optimistic bubble before they can race it.
                notify_message_id(&ui_sender, &jid_str, local_id, &msg_id).await;
                record_outgoing(&chat_store, &jid, &msg_id, &message).await;
                let options = whatsapp_rust::SendOptions::default().with_message_id(msg_id.clone());
                match client
                    .send_message_with_options(jid.clone(), message, options)
                    .await
                {
                    Ok(result) => {
                        info!("Message sent successfully: {}", result.message_id);
                    }
                    Err(e) => {
                        error!("Failed to send message {}: {}", msg_id, e);
                        mark_send_failed(&chat_store, &jid, &msg_id).await;
                        notify_send_failed(&ui_sender, &jid_str, &msg_id, e.to_string()).await;
                    }
                }
            } else {
                error!("Client not available for sending message");
                // The bubble still carries its local id (no rename ran)
                notify_send_failed(
                    &ui_sender,
                    &jid_str,
                    &local_id,
                    "client not available".to_string(),
                )
                .await;
            }
        })
    }

    /// Download media using DownloadableMedia info
    /// Returns a oneshot receiver that will contain the result
    pub fn download_downloadable_media(
        &self,
        downloadable: DownloadableMedia,
    ) -> tokio::sync::oneshot::Receiver<Result<Vec<u8>, String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let client_handle = self.client_handle.clone();

        self.exec.spawn(async move {
            // Clone the Arc and release the mutex: a slow network call
            // here must not queue every other client action behind it.
            let client = client_handle.lock().await.clone();
            if let Some(client) = client {
                info!(
                    "Downloading media: {} bytes expected",
                    downloadable.file_length
                );
                match client.download(&downloadable).await {
                    Ok(data) => {
                        info!("Media downloaded successfully: {} bytes", data.len());
                        let _ = tx.send(Ok(data));
                    }
                    Err(e) => {
                        error!("Failed to download media: {}", e);
                        let _ = tx.send(Err(e.to_string()));
                    }
                }
            } else {
                let _ = tx.send(Err("Client not available".to_string()));
            }
        });

        rx
    }

    /// Send a PTT audio message to a chat
    /// Returns a handle that completes when the send has run its course; see
    /// [`WhatsAppClient::send_message`] for why.
    pub fn send_audio_message(
        &self,
        jid_str: &str,
        audio_data: Vec<u8>,
        duration_secs: u32,
        waveform: Vec<u8>,
        local_id: String,
        quoted: Option<oxidezap_core::QuotedMessage>,
    ) -> Task<()> {
        let chat_store = self.chat_store.clone();
        let ui_sender = self.ui_sender.clone();
        let client_handle = self.client_handle.clone();
        let jid_str = jid_str.to_string();

        self.exec.spawn(async move {
            let jid: Jid = match jid_str.parse() {
                Ok(j) => j,
                Err(e) => {
                    error!("Invalid JID {}: {}", observe_str(&jid_str), e);
                    // The optimistic bubble still carries its local id;
                    // without this it would sit unsent with no indicator.
                    notify_send_failed(&ui_sender, &jid_str, &local_id, e.to_string()).await;
                    return;
                }
            };

            // Clone the Arc and release the mutex: a slow network call
            // here must not queue every other client action behind it.
            let client = client_handle.lock().await.clone();
            if let Some(client) = client {
                let upload_result = match client
                    .upload(audio_data, DownloadMediaType::Audio, Default::default())
                    .await
                {
                    Ok(resp) => resp,
                    Err(e) => {
                        error!("Failed to upload audio: {}", e);
                        // Bubble still carries the local id at this point.
                        notify_send_failed(&ui_sender, &jid_str, &local_id, e.to_string()).await;
                        return;
                    }
                };

                info!("Audio uploaded successfully");

                let audio_message = wa::message::AudioMessage {
                    url: Some(upload_result.url),
                    direct_path: Some(upload_result.direct_path),
                    media_key: Some(upload_result.media_key.to_vec()),
                    file_sha256: Some(upload_result.file_sha256.to_vec()),
                    file_enc_sha256: Some(upload_result.file_enc_sha256.to_vec()),
                    file_length: Some(upload_result.file_length),
                    mimetype: Some("audio/ogg; codecs=opus".to_string()),
                    seconds: Some(duration_secs),
                    ptt: Some(true), // This marks it as a voice message
                    waveform: Some(waveform),
                    ..Default::default()
                };

                // Quoted the same way the text path does it, because a voice
                // note answering a message is a reply — the recipient should
                // see the quote bar over it, not a bare note.
                let message = match &quoted {
                    Some(quoted) => wa::Message {
                        audio_message: whatsapp_rust::buffa::MessageField::some(
                            wa::message::AudioMessage {
                                context_info: whatsapp_rust::buffa::MessageField::some(
                                    quote_context(quoted),
                                ),
                                ..audio_message
                            },
                        ),
                        ..Default::default()
                    },
                    None => wa::Message {
                        audio_message: whatsapp_rust::buffa::MessageField::some(audio_message),
                        ..Default::default()
                    },
                };

                // Same ordering as the text path: record before sending so
                // the ack can't precede the row in the writer queue.
                let msg_id = client.generate_message_id();
                notify_message_id(&ui_sender, &jid_str, local_id, &msg_id).await;
                record_outgoing(&chat_store, &jid, &msg_id, &message).await;
                let options = whatsapp_rust::SendOptions::default().with_message_id(msg_id.clone());
                match client
                    .send_message_with_options(jid.clone(), message, options)
                    .await
                {
                    Ok(result) => {
                        info!("Audio message sent successfully: {}", result.message_id);
                    }
                    Err(e) => {
                        error!("Failed to send audio message {}: {}", msg_id, e);
                        mark_send_failed(&chat_store, &jid, &msg_id).await;
                        notify_send_failed(&ui_sender, &jid_str, &msg_id, e.to_string()).await;
                    }
                }
            } else {
                error!("Client not available for sending audio message");
                // The bubble still carries its local id (no rename ran)
                notify_send_failed(
                    &ui_sender,
                    &jid_str,
                    &local_id,
                    "client not available".to_string(),
                )
                .await;
            }
        })
    }

    /// Send "composing" chat state (typing indicator)
    pub fn send_composing(&self, jid_str: &str) {
        let client_handle = self.client_handle.clone();
        let jid_str = jid_str.to_string();

        self.exec.spawn(async move {
            let jid: Jid = match jid_str.parse() {
                Ok(j) => j,
                Err(e) => {
                    error!("Invalid JID {}: {}", observe_str(&jid_str), e);
                    return;
                }
            };

            let client = client_handle.lock().await.clone();
            if let Some(client) = client
                && let Err(e) = client.chatstate().send_composing(&jid).await
            {
                warn!("Failed to send composing state: {}", e);
            }
        });
    }

    /// Send "paused" chat state (stopped typing)
    pub fn send_paused(&self, jid_str: &str) {
        let client_handle = self.client_handle.clone();
        let jid_str = jid_str.to_string();

        self.exec.spawn(async move {
            let jid: Jid = match jid_str.parse() {
                Ok(j) => j,
                Err(e) => {
                    error!("Invalid JID {}: {}", observe_str(&jid_str), e);
                    return;
                }
            };

            let client = client_handle.lock().await.clone();
            if let Some(client) = client
                && let Err(e) = client.chatstate().send_paused(&jid).await
            {
                warn!("Failed to send paused state: {}", e);
            }
        });
    }

    /// Send read receipts to mark messages as read
    ///
    /// # Arguments
    /// * `chat_jid_str` - The JID of the chat (e.g., "123456@s.whatsapp.net")
    /// * `messages` - List of (message_id, sender_jid_string) tuples
    ///
    /// Returns a handle that completes when the receipts have gone out; see
    /// [`WhatsAppClient::send_message`] for why.
    pub fn send_read_receipts(
        &self,
        chat_jid_str: &str,
        messages: Vec<(String, String)>,
    ) -> Task<()> {
        let client_handle = self.client_handle.clone();
        let chat_jid_str = chat_jid_str.to_string();

        self.exec.spawn(async move {
            // Inside the task, not before it: the caller gets a handle it can
            // await either way, rather than having to special-case nothing to
            // do.
            if messages.is_empty() {
                return;
            }

            let chat_jid: Jid = match chat_jid_str.parse() {
                Ok(j) => j,
                Err(e) => {
                    error!("Invalid chat JID {}: {}", observe_str(&chat_jid_str), e);
                    return;
                }
            };

            let parsed_messages: Vec<(String, Jid)> = messages
                .into_iter()
                .filter_map(|(msg_id, sender_str)| {
                    sender_str
                        .parse::<Jid>()
                        .inspect_err(|e| {
                            warn!("Invalid sender JID {}: {}", observe_str(&sender_str), e)
                        })
                        .ok()
                        .map(|jid| (msg_id, jid))
                })
                .collect();

            if parsed_messages.is_empty() {
                return;
            }

            // Only group/broadcast receipts carry a participant (matches
            // whatsmeow/WA Web); a plain DM receipt must not.
            let needs_participant = participant_keyed_chat(&chat_jid);

            // Clone the Arc and release the mutex: a slow network call
            // here must not queue every other client action behind it.
            let client = client_handle.lock().await.clone();
            if let Some(client) = client {
                let mut by_sender: HashMap<Jid, Vec<String>> = HashMap::new();
                for (msg_id, sender) in parsed_messages {
                    by_sender.entry(sender).or_default().push(msg_id);
                }
                for (sender, msg_ids) in by_sender {
                    let id_refs: Vec<&str> = msg_ids.iter().map(String::as_str).collect();
                    if let Err(e) = client
                        .mark_as_read(&chat_jid, needs_participant.then_some(&sender), &id_refs)
                        .await
                    {
                        warn!("Failed to mark messages as read: {}", e);
                    }
                }
            } else {
                error!("Client not available for sending read receipts");
            }
        })
    }

    /// Ask for a full history reload.
    ///
    /// For a front end that has just attached: nothing in the store has
    /// changed, so the invalidation stream has nothing to say, and without
    /// this the new arrival would sit empty until the next message arrived.
    pub fn reload_history(&self) {
        self.reload.notify_one();
    }

    /// Remember that these status updates have been watched.
    ///
    /// Local, and deliberately so: WhatsApp's own answer is a status read
    /// receipt, which is a privacy setting the library does not expose. What
    /// this buys is the honest half — the ring stops claiming there is
    /// something new here, and it stays stopped across a restart, which is
    /// what a front end's own memory of it could not do.
    ///
    /// The broadcast's unread cursor is not that answer: it counts one chat
    /// carrying everybody's updates, so clearing it would watch every
    /// contact's run at once.
    ///
    /// Returns a handle that answers whether the row reached the store. The
    /// caller waits for it: a view is the whole point of the request and
    /// there is no retry — the window has already drawn the ring as watched —
    /// so "accepted" has to mean "written", not "queued behind a teardown
    /// that may cancel it".
    ///
    /// A row that moves invalidates its chat, so the reloader republishes the
    /// broadcast and every attached front end learns about the view through
    /// the history it already knows how to recover. Nothing has to be told
    /// separately, and nothing is lost if a client is behind.
    pub fn mark_status_watched(&self, message_ids: Vec<String>) -> Task<bool> {
        let chat_store = self.chat_store.clone();
        self.exec.spawn(async move {
            let Some(store) = chat_store.lock().await.clone() else {
                warn!("no chat store yet; a watched status update was not recorded");
                return false;
            };
            let Ok(broadcast) = oxidezap_core::STATUS_BROADCAST_JID.parse::<Jid>() else {
                warn!("the status broadcast address does not parse");
                return false;
            };
            // Enqueued and then awaited: the write goes through the same
            // ordered queue as the insert that created the row it targets,
            // and `flush` is what turns "queued" into "committed", which is
            // what the caller is waiting to hear.
            let written = match store.mark_status_watched(&broadcast, message_ids) {
                Ok(()) => store.flush().await,
                Err(e) => Err(e),
            };
            match written {
                Ok(()) => true,
                Err(e) => {
                    warn!("failed to record watched status updates: {e}");
                    false
                }
            }
        })
    }

    /// Synchronize a bounded read action so newer messages remain unread.
    ///
    /// Returns a handle that completes when the action has run; see
    /// [`WhatsAppClient::send_message`] for why.
    pub fn mark_chat_read(
        &self,
        chat_jid_str: &str,
        last_displayed: Option<ReadBoundary>,
    ) -> Task<()> {
        let client_handle = self.client_handle.clone();
        let chat_jid_str = chat_jid_str.to_string();

        self.exec.spawn(async move {
            let chat_jid: Jid = match chat_jid_str.parse() {
                Ok(j) => j,
                Err(e) => {
                    error!("Invalid chat JID {}: {}", observe_str(&chat_jid_str), e);
                    return;
                }
            };
            let Some(client) = client_handle.lock().await.clone() else {
                error!("Client not available for marking chat read");
                return;
            };
            let range = last_displayed.map(|boundary| read_message_range(&chat_jid, boundary));
            if let Err(e) = client
                .chat_actions()
                .mark_chat_as_read(&chat_jid, true, range)
                .await
            {
                warn!("Failed to mark chat {} as read: {}", chat_jid.observe(), e);
            }
        })
    }
}

/// Tell the UI which real id a just-sent optimistic bubble got.
async fn notify_message_id(
    ui_sender: &UiEventSender,
    chat_jid: &str,
    local_id: String,
    message_id: &str,
) {
    if let Some(tx) = ui_sender.lock().await.as_ref() {
        let _ = tx.send(UiEvent::MessageIdAssigned {
            chat_jid: chat_jid.to_string(),
            local_id,
            message_id: message_id.to_string(),
        });
    }
}

/// Tell the UI a send failed so the bubble doesn't sit pending forever.
async fn notify_send_failed(
    ui_sender: &UiEventSender,
    chat_jid: &str,
    message_id: &str,
    reason: String,
) {
    if let Some(tx) = ui_sender.lock().await.as_ref() {
        let _ = tx.send(UiEvent::SendFailed {
            chat_jid: chat_jid.to_string(),
            message_id: message_id.to_string(),
            reason,
        });
    }
}

/// Best-effort durable record of a message this client just sent; the UI's
/// optimistic bubble is independent of this.
async fn record_outgoing(
    chat_store: &ChatStoreHandle,
    jid: &Jid,
    message_id: &str,
    message: &wa::Message,
) {
    if let Some(store) = chat_store.lock().await.as_ref()
        && let Err(e) = store.record_outgoing(
            jid,
            message_id,
            message,
            whatsapp_rust::wacore::time::now_utc(),
        )
    {
        warn!("Failed to record outgoing message {}: {e}", message_id);
    }
}

/// Best-effort failure mark on the durable row a client-side send error
/// orphans at Pending (no server nack will come to fail it), so a restart
/// hydrates the bubble with its failure indicator instead of grey ticks.
async fn mark_send_failed(chat_store: &ChatStoreHandle, jid: &Jid, message_id: &str) {
    if let Some(store) = chat_store.lock().await.as_ref()
        && let Err(e) = store.mark_send_failed(jid, message_id)
    {
        warn!("Failed to mark send {} as failed: {e}", message_id);
    }
}

/// Map a PN chat JID to its LID form when a mapping is known, so the same user
/// doesn't split into two chats (PN vs LID addressing).
async fn normalize_chat_jid(client: &Client, jid_str: &str) -> String {
    let Ok(jid) = jid_str.parse::<Jid>() else {
        return jid_str.to_string();
    };
    if !jid.is_pn() {
        return jid_str.to_string();
    }
    match client.get_lid_pn_entry(&jid).await {
        Ok(Some(entry)) => format!("{}@lid", entry.lid),
        _ => jid_str.to_string(),
    }
}

impl Drop for WhatsAppClient {
    fn drop(&mut self) {
        // A dropped wrapper can never be shut down explicitly anymore; free
        // its loop instead of leaking the executor + DB pool.
        self.shutdown.notify_one();
    }
}

/// An error and everything under it, on one line.
///
/// The store's errors are categories with the cause hung off `source`, and a
/// browser has no database file to open afterwards and inspect — so a report
/// that stops at the category is a report nobody can act on.
fn because(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        use std::fmt::Write as _;
        let _ = write!(text, ": {cause}");
        source = cause.source();
    }
    text
}
