//! WhatsApp client wrapper for UI integration

/// Voice calls, which are the one part of the session a page cannot run.
mod calls;

use calls::CallRegistry;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use log::{debug, error, info, warn};
use oxidezap_chat_store::{ChatEntry, ChatStore, StoreChange};
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
    Availability, CallVideoFrame, Chat, ChatMessage, ComposingKind, DownloadableMedia,
    IncomingCall, MediaContent, MediaType, MessageStatus, SystemNotice, UiEvent, VideoStream,
    fallback_chat_name,
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

/// Helper struct for building DownloadableMedia from common message fields
struct DownloadableBuilder<'a> {
    direct_path: Option<&'a str>,
    media_key: Option<&'a [u8]>,
    file_enc_sha256: Option<&'a [u8]>,
    file_length: Option<u64>,
    mime_type: &'a str,
    duration_secs: Option<u32>,
    download_type: DownloadMediaType,
}

impl<'a> DownloadableBuilder<'a> {
    /// Try to build a DownloadableMedia from the provided fields.
    /// Returns None if any required field (direct_path, media_key, file_enc_sha256) is missing.
    fn build(self) -> Option<DownloadableMedia> {
        let direct_path = self.direct_path?;
        let media_key = self.media_key?;
        let file_enc_sha256 = self.file_enc_sha256?;

        Some(DownloadableMedia {
            direct_path: direct_path.to_string(),
            media_key: media_key.to_vec(),
            file_enc_sha256: file_enc_sha256.to_vec(),
            file_length: self.file_length.unwrap_or(0),
            mime_type: self.mime_type.to_string(),
            duration_secs: self.duration_secs,
            download_type: self.download_type,
        })
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

/// What a history load has to say: the chats, whether they are the whole
/// list, and where the list continues.
///
/// The third is what makes a front end's first "load more" a page it does not
/// already have. A load has already walked the store's order to its limit, so
/// the position it stopped at costs nothing to carry; asking for it instead
/// is a hundred rows re-read, re-serialized and re-merged to learn one
/// string.
struct LoadedHistory {
    chats: Vec<oxidezap_core::Chat>,
    complete: bool,
    next: Option<String>,
}

impl LoadedHistory {
    /// The event a front end reads this as. One place, so a load cannot reach
    /// a window having quietly dropped where it ended.
    fn into_event(self) -> UiEvent {
        UiEvent::HistoryLoaded {
            chats: self.chats,
            complete: self.complete,
            next: self.next,
        }
    }
}

/// One page of something, and where to continue.
///
/// `next` is a token this crate writes and this crate reads. Nothing outside
/// it may parse one: what a page is ordered by is a fact about the store's
/// indexes, and a caller that took the token apart would be a second
/// implementation of that order. `None` is the end of the list — there is no
/// position after the last row, so absence is the only honest way to say so.
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
}

/// The cursor for continuing a conversation before `message`.
fn message_cursor(message: &oxidezap_chat_store::StoredMessage) -> String {
    let cursor = oxidezap_chat_store::MessageCursor::from(message);
    format!("m1:{}:{}", cursor.timestamp_ms, cursor.seq)
}

fn parse_message_cursor(token: &str) -> Option<oxidezap_chat_store::MessageCursor> {
    let mut parts = token.strip_prefix("m1:")?.split(':');
    Some(oxidezap_chat_store::MessageCursor {
        timestamp_ms: parts.next()?.parse().ok()?,
        seq: parts.next()?.parse().ok()?,
    })
}

/// The cursor for continuing the chat list after `entry`.
///
/// The JID goes last and is not split on, because a device address carries a
/// colon of its own (`5599…:57`).
/// The one chat nobody opens as a conversation.
fn is_status_broadcast(entry: &ChatEntry) -> bool {
    entry.jid.to_non_ad_string() == oxidezap_core::STATUS_BROADCAST_JID
}

fn chat_cursor(entry: &ChatEntry) -> String {
    let cursor = oxidezap_chat_store::ChatCursor::from(entry);
    let pinned = cursor
        .pinned_at_ms
        .map_or_else(|| "-".to_string(), |t| t.to_string());
    format!("c1:{pinned}:{}:{}", cursor.last_message_ts, cursor.jid)
}

fn parse_chat_cursor(token: &str) -> Option<oxidezap_chat_store::ChatCursor> {
    let mut parts = token.strip_prefix("c1:")?.splitn(3, ':');
    // An unreadable pin is an unreadable cursor, not an unpinned chat: read as
    // `None` it is a valid position in the wrong half of the order, and the
    // next page silently skips or repeats conversations.
    let pinned_at_ms = match parts.next()? {
        "-" => None,
        pinned => Some(pinned.parse().ok()?),
    };
    Some(oxidezap_chat_store::ChatCursor {
        pinned_at_ms,
        last_message_ts: parts.next()?.parse().ok()?,
        jid: parts.next()?.to_string(),
    })
}

pub type ReadBoundary = (i64, Vec<(String, bool, Option<String>)>);

fn participant_keyed_chat(jid: &Jid) -> bool {
    jid.is_group() || jid.is_broadcast_list() || jid.is_status_broadcast()
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
        info!("Opening data database");
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
        let chat_store = match ChatStore::new(&backend).await {
            Ok(store) => store,
            Err(e) => {
                error!("Failed to open chat store: {}", because(&e));
                let _ = ui_tx.send(UiEvent::Error(format!("Database error: {}", e)));
                return;
            }
        };
        {
            let mut guard = chat_store_handle.lock().await;
            *guard = Some(chat_store.clone());
        }
        info!("SQLite backend + chat store initialized.");

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
        crate::relay::install(&bot.client());

        // Hydrate the UI from durable history before the network is even up
        // (bot.run() is what connects). The client is needed here so hydrated
        // JIDs normalize through the same PN->LID mapping live events use.
        match Self::load_history(&chat_store, &bot.client(), &names).await {
            Ok(loaded) if !loaded.chats.is_empty() => {
                // The one hydration worth an info line: the reloads that
                // follow are routine and say so at debug.
                info!("Hydrated {} chats from durable history", loaded.chats.len());
                let _ = ui_tx.send(loaded.into_event());
            }
            Ok(_) => {}
            Err(e) => warn!("Failed to load chat history: {e}"),
        }

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
                    let _ = ui_tx.send(UiEvent::CallEnded(call_id.clone()));
                }
                CallAction::Terminate { call_id, .. } => {
                    info!("Call {} terminated by peer", call_id);
                    calls.ended_remotely(call_id);
                    let _ = ui_tx.send(UiEvent::CallEnded(call_id.clone()));
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
                let _ = ui_tx.send(UiEvent::CallEnded(missed.call_id.clone()));
            }
            Event::CallEndedElsewhere(ended) => {
                info!("Call {} handled on another device", ended.call_id);
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
        let media_result = Self::try_extract_media(base_msg, client, eager).await;

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

    /// The eager fetch, or nothing when this is not the moment for one.
    async fn fetch_now<T: whatsapp_rust::wacore::download::Downloadable>(
        client: &Arc<Client>,
        media: &T,
        media_name: &str,
        eager: bool,
        file_length: Option<u64>,
    ) -> Option<Vec<u8>> {
        if !Self::worth_fetching_now(eager, file_length) {
            return None;
        }
        Self::download_media(client, media, media_name).await
    }

    /// Helper to download media with logging
    async fn download_media<T: whatsapp_rust::wacore::download::Downloadable>(
        client: &Arc<Client>,
        media: &T,
        media_name: &str,
    ) -> Option<Vec<u8>> {
        info!("Downloading {}...", media_name);
        match client.download(media).await {
            Ok(data) => {
                info!(
                    "{} downloaded successfully: {} bytes",
                    media_name,
                    data.len()
                );
                Some(data)
            }
            Err(e) => {
                warn!("Failed to download {}: {}", media_name, e);
                None
            }
        }
    }

    /// Most bytes a picture may be worth fetching before anybody has asked
    /// for it.
    ///
    /// A photo sent through WhatsApp is a fraction of this; past it the
    /// message keeps its thumbnail and its download metadata, which is what
    /// the renderer already draws for a video, and the full bytes arrive when
    /// somebody opens it.
    const EAGER_MEDIA_BYTES: u64 = 4 * 1024 * 1024;

    /// Whether media of this size is worth fetching before anybody asked.
    fn worth_fetching_now(eager: bool, file_length: Option<u64>) -> bool {
        eager && file_length.is_none_or(|len| len <= Self::EAGER_MEDIA_BYTES)
    }

    /// Try to extract media from a message, fetching the bytes when they are
    /// worth having before anybody has asked for them.
    ///
    /// Not fetching them is the same shape as failing to: the thumbnail is
    /// what shows and the download metadata is what makes the full bytes
    /// retryable.
    async fn try_extract_media(
        msg: &wa::Message,
        _client: &Arc<Client>,
        eager: bool,
    ) -> Option<MediaContent> {
        // Check for sticker message
        if let Some(sticker) = effective_sticker(msg) {
            let mime = sticker
                .mimetype
                .clone()
                .unwrap_or_else(|| "image/webp".to_string());
            let downloadable = DownloadableBuilder {
                direct_path: sticker.direct_path.as_deref(),
                media_key: sticker.media_key.as_deref(),
                file_enc_sha256: sticker.file_enc_sha256.as_deref(),
                file_length: sticker.file_length,
                mime_type: &mime,
                duration_secs: None,
                download_type: DownloadMediaType::Sticker,
            }
            .build();
            // Same rule as the image path below: a failed eager download
            // degrades to the thumbnail (and stays retryable through the
            // download metadata) instead of the message losing its media.
            let (data, mime_type, is_animated, data_is_preview) = match Self::fetch_now(
                _client,
                sticker,
                "sticker",
                eager,
                sticker.file_length,
            )
            .await
            {
                Some(data) => (data, mime, sticker.is_animated.unwrap_or(false), false),
                None => {
                    let still = still_preview(
                        thumbnail_bytes(sticker.png_thumbnail.as_deref()),
                        "image/png",
                        mime,
                        downloadable.is_some(),
                    );
                    (
                        still.data,
                        still.mime,
                        // What the sticker *is*, not what the still is: the
                        // flag describes the file that replaces it, and
                        // `data_is_preview` beside it says which of the two
                        // is in hand.
                        sticker.is_animated.unwrap_or(false),
                        still.is_preview,
                    )
                }
            };
            if data.is_empty() && downloadable.is_none() {
                return None;
            }
            info!(
                "Sticker: mime={}, is_animated={}, is_lottie={}, size={} bytes",
                mime_type,
                is_animated,
                sticker.is_lottie.unwrap_or(false),
                data.len()
            );
            return Some(MediaContent {
                media_type: MediaType::Sticker,
                data: Arc::new(data),
                cache_key: None,
                mime_type,
                width: sticker.width,
                height: sticker.height,
                caption: None,
                file_name: None,
                downloadable,
                is_animated,
                duration_secs: None,
                data_is_preview,
                waveform: None,
            });
        }

        // Check for image message
        if let Some(image) = msg.image_message.as_option() {
            let downloadable = DownloadableBuilder {
                direct_path: image.direct_path.as_deref(),
                media_key: image.media_key.as_deref(),
                file_enc_sha256: image.file_enc_sha256.as_deref(),
                file_length: image.file_length,
                mime_type: image.mimetype.as_deref().unwrap_or("image/jpeg"),
                duration_secs: None,
                download_type: DownloadMediaType::Image,
            }
            .build();
            // A failed eager download keeps the metadata: the thumbnail shows
            // now and the full image stays retryable, instead of the message
            // degrading to a plain text row for the whole session.
            let (data, mime_type, data_is_preview) =
                match Self::fetch_now(_client, image, "image", eager, image.file_length).await {
                    Some(data) => (
                        data,
                        image
                            .mimetype
                            .clone()
                            .unwrap_or_else(|| "image/jpeg".to_string()),
                        false,
                    ),
                    None => {
                        let still = still_preview(
                            thumbnail_bytes(image.jpeg_thumbnail.as_deref()),
                            "image/jpeg",
                            image
                                .mimetype
                                .clone()
                                .unwrap_or_else(|| "image/jpeg".to_string()),
                            downloadable.is_some(),
                        );
                        (still.data, still.mime, still.is_preview)
                    }
                };
            if data.is_empty() && downloadable.is_none() {
                return None;
            }
            return Some(MediaContent {
                media_type: MediaType::Image,
                data: Arc::new(data),
                cache_key: None,
                mime_type,
                width: image.width,
                height: image.height,
                caption: image.caption.clone(),
                file_name: None,
                downloadable,
                is_animated: false,
                duration_secs: None,
                data_is_preview,
                waveform: None,
            });
        }

        // Check for video message - store thumbnail for preview, metadata for
        // download. PTVs (round video notes) are the same proto type in a
        // different field and play like any other video.
        if let Some(video) = msg
            .ptv_message
            .as_option()
            .or(msg.video_message.as_option())
        {
            // Use thumbnail for display, or empty vec if none
            let thumbnail_data = video
                .jpeg_thumbnail
                .as_ref()
                .filter(|t| !t.is_empty())
                .cloned()
                .unwrap_or_default();

            // Build downloadable info using helper
            let downloadable = DownloadableBuilder {
                direct_path: video.direct_path.as_deref(),
                media_key: video.media_key.as_deref(),
                file_enc_sha256: video.file_enc_sha256.as_deref(),
                file_length: video.file_length,
                mime_type: video.mimetype.as_deref().unwrap_or("video/mp4"),
                duration_secs: video.seconds,
                download_type: DownloadMediaType::Video,
            }
            .build();

            // Only return if we have either thumbnail or downloadable info
            if !thumbnail_data.is_empty() || downloadable.is_some() {
                // A video's `data` is never the video: these are the JPEG
                // bytes of its poster frame, which is what the mime type
                // beside them already says. Calling them the full media wrote
                // a thumbnail under the full-video cache key, and every later
                // read of that key handed back a still.
                let data_is_preview = !thumbnail_data.is_empty();
                return Some(MediaContent {
                    media_type: MediaType::Video,
                    data: Arc::new(thumbnail_data),
                    cache_key: None,
                    mime_type: "image/jpeg".to_string(), // Thumbnail is JPEG
                    width: video.width,
                    height: video.height,
                    caption: video.caption.clone(),
                    file_name: None,
                    downloadable,
                    is_animated: false,
                    duration_secs: video.seconds,
                    data_is_preview,
                    waveform: None,
                });
            }
        }

        // Check for audio message - lazy load, only download when user clicks play
        if let Some(audio) = msg.audio_message.as_option() {
            let default_mime = "audio/ogg; codecs=opus";
            let mime_type = audio.mimetype.as_deref().unwrap_or(default_mime);

            // Build downloadable info using helper
            let downloadable = DownloadableBuilder {
                direct_path: audio.direct_path.as_deref(),
                media_key: audio.media_key.as_deref(),
                file_enc_sha256: audio.file_enc_sha256.as_deref(),
                file_length: audio.file_length,
                mime_type,
                duration_secs: audio.seconds,
                download_type: DownloadMediaType::Audio,
            }
            .build();

            // Only return if we have downloadable info
            if downloadable.is_some() {
                return Some(MediaContent {
                    media_type: MediaType::Audio,
                    data: Arc::new(vec![]), // Empty until downloaded
                    cache_key: None,
                    mime_type: mime_type.to_string(),
                    width: None,
                    height: None,
                    caption: None,
                    file_name: None,
                    downloadable,
                    is_animated: false,
                    duration_secs: audio.seconds,
                    data_is_preview: false,
                    // Drawn before a byte of audio is fetched, which is the
                    // point: the shape of a voice note is most useful while
                    // deciding whether to play it.
                    waveform: audio
                        .waveform
                        .as_deref()
                        .filter(|w| !w.is_empty())
                        .map(|w| Arc::new(w.to_vec())),
                });
            }
        }

        // Check for document message (no eager download, just metadata)
        if let Some(doc) = msg.document_message.as_option() {
            let mime = doc.mimetype.clone().unwrap_or_default();
            let downloadable = DownloadableBuilder {
                direct_path: doc.direct_path.as_deref(),
                media_key: doc.media_key.as_deref(),
                file_enc_sha256: doc.file_enc_sha256.as_deref(),
                file_length: doc.file_length,
                mime_type: &mime,
                duration_secs: None,
                download_type: DownloadMediaType::Document,
            }
            .build();
            return Some(MediaContent {
                media_type: MediaType::Document,
                data: Arc::new(vec![]),
                cache_key: None,
                mime_type: mime,
                width: None,
                height: None,
                caption: doc.caption.clone(),
                file_name: doc.file_name.clone(),
                downloadable,
                is_animated: false,
                duration_secs: None,
                data_is_preview: false,
                waveform: None,
            });
        }

        None
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

    /// One page of a chat's messages, older than `before`.
    ///
    /// The read a front end makes when it opens a conversation and again when
    /// it scrolls back through one. Hydrated exactly as the attach load
    /// hydrates its rows — reactions, sender names — because a bubble drawn
    /// from a page and the same bubble drawn from a load must say the same
    /// thing.
    ///
    /// The cursor is this side's to write and to read: see [`Page`].
    pub fn load_messages(
        &self,
        jid: String,
        before: Option<String>,
        limit: i64,
    ) -> Task<Result<Page<ChatMessage>, String>> {
        let chat_store = self.chat_store.clone();
        let client_handle = self.client_handle.clone();
        let names = self.names.clone();
        self.exec.spawn(async move {
            let Some(store) = chat_store.lock().await.clone() else {
                return Err("no chat store yet".to_string());
            };
            let Some(client) = client_handle.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let Some(names) = names.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            Self::message_page(&store, &client, &names, jid, before, limit).await
        })
    }

    async fn message_page(
        store: &Arc<ChatStore>,
        client: &Arc<Client>,
        names: &NameBook,
        jid: String,
        before: Option<String>,
        limit: i64,
    ) -> Result<Page<ChatMessage>, String> {
        let chat: Jid = jid.parse().map_err(|_| "not a chat address".to_string())?;
        let before = before
            .map(|cursor| parse_message_cursor(&cursor).ok_or("unreadable cursor".to_string()))
            .transpose()?;

        let limit = limit.clamp(1, Self::MESSAGE_PAGE);
        // The page and how much of the unread tail it owes, out of one
        // snapshot: asked separately, a message committed between the two
        // raises the counter without appearing in the page, and the tail then
        // reaches a row further back than the page justifies — one already
        // read, advertised as owing a receipt.
        let (mut page, unread) = store
            .page_with_unread(&chat, before, limit)
            .await
            .map_err(|e| e.to_string())?;
        // A page shorter than it asked for is the start of the
        // conversation: there is nothing older to name a cursor with.
        let next = ((page.len() as i64) == limit)
            .then(|| page.last().map(message_cursor))
            .flatten();
        page.reverse(); // the store returns newest-first; a timeline is drawn the other way
        let mut messages: Vec<ChatMessage> = page.into_iter().map(stored_to_chat_message).collect();
        Self::hydrate_reactions(store, client, names, &chat, &mut messages).await;
        Self::canonicalize_quoted_authors(client, names, &mut messages).await;
        if chat.is_group() || chat.is_status_broadcast() {
            Self::hydrate_sender_names(
                store,
                client,
                &mut messages,
                names,
                chat.is_status_broadcast(),
            )
            .await;
        }
        // Exactly what the attach load does to its rows, which is what the
        // paragraph above promises: a page hydrated any other way is one whose
        // unread tail nobody ever sends a receipt for.
        mark_unread_tail(&mut messages, unread.clamp(0, u32::MAX as i64) as u32);
        Ok(Page {
            items: messages,
            next,
        })
    }

    /// One page of the chat list, after `after`.
    ///
    /// Rows, not conversations: each carries the newest message the list
    /// previews from and nothing else. What a front end does with the rest of
    /// a chat is ask for it.
    pub fn load_chats(
        &self,
        after: Option<String>,
        limit: i64,
    ) -> Task<Result<Page<oxidezap_core::Chat>, String>> {
        let chat_store = self.chat_store.clone();
        let client_handle = self.client_handle.clone();
        let names = self.names.clone();
        self.exec.spawn(async move {
            let Some(store) = chat_store.lock().await.clone() else {
                return Err("no chat store yet".to_string());
            };
            let Some(client) = client_handle.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let Some(names) = names.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let after = after
                .map(|cursor| parse_chat_cursor(&cursor).ok_or("unreadable cursor".to_string()))
                .transpose()?;

            let limit = limit.clamp(1, Self::CHAT_PAGE);
            let entries = store
                .chats_page(false, after, limit)
                .await
                .map_err(|e| e.to_string())?;
            // Off the page as it was read, before the aliases below join it:
            // where the list continues is a position in the store's own order,
            // and a row pulled in from outside the page is not one.
            let next = ((entries.len() as i64) == limit)
                .then(|| entries.last().map(chat_cursor))
                .flatten();
            let entries = Self::with_alias_rows(&store, &client, &names, entries).await;
            // Sized exactly as the attach load sizes it, and for the same
            // reasons: the row previews from its newest message, a read owes
            // a receipt per unread message rather than one for the chat, and
            // the status broadcast is nobody's conversation to open. A page
            // that carried the newest row alone let a window read a chat
            // whose older unread messages then went unacknowledged.
            let chats = Self::hydrate_entries(&store, &client, &names, entries, Self::attach_page)
                .await
                .map_err(|e| e.to_string())?;
            Ok(Page { items: chats, next })
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

fn read_message_range(
    chat_jid: &Jid,
    (ts_secs, ids): ReadBoundary,
) -> wa::sync_action_value::SyncActionMessageRange {
    use whatsapp_rust::features::{message_key, message_range};

    let messages = ids
        .into_iter()
        .filter_map(|(id, from_me, sender)| {
            let participant = if participant_keyed_chat(chat_jid) && !from_me {
                let sender = sender?;
                match sender.parse::<Jid>() {
                    Ok(jid) => Some(jid),
                    Err(e) => {
                        warn!("Invalid chat participant {}: {e}", observe_str(&sender));
                        return None;
                    }
                }
            } else {
                None
            };
            Some((
                message_key(id, chat_jid, from_me, participant.as_ref()),
                ts_secs,
            ))
        })
        .collect();

    message_range(ts_secs, None, messages)
}

fn merge_alias_history_messages(
    chat: &mut Chat,
    mut messages: Vec<ChatMessage>,
    alias_unread: u32,
) {
    // Alias rows may be disjoint or repeated; only loaded message IDs prove
    // that two unread counters overlap.
    let existing_unread_ids: HashSet<String> = chat
        .messages
        .iter()
        .filter(|message| !message.is_from_me && !message.is_read)
        .map(|message| message.id.clone())
        .collect();
    let duplicate_unread = messages
        .iter()
        .filter(|message| {
            !message.is_from_me && !message.is_read && existing_unread_ids.contains(&message.id)
        })
        .count() as u32;

    for message in &mut messages {
        if !message.is_from_me && existing_unread_ids.contains(&message.id) {
            message.is_read = false;
        }
    }
    for message in messages {
        chat.insert_history_message(message);
    }

    let visible_unread = chat
        .messages
        .iter()
        .filter(|message| !message.is_from_me && !message.is_read)
        .count() as u32;
    chat.unread_count = chat
        .unread_count
        .saturating_add(alias_unread)
        .saturating_sub(duplicate_unread)
        .max(visible_unread);
}

/// The updates in `page` whose stored ack says they were watched here.
///
/// `Read` on an incoming row means exactly that and nothing else: the column
/// is written once at insert as `Delivered`, peer receipts only advance our
/// own messages, and a redelivery refreshes content without touching it. The
/// same field WhatsApp Web moves to `ACK.READ` when a status is viewed.
fn watched_ids(page: &[oxidezap_chat_store::StoredMessage]) -> impl Iterator<Item = String> + '_ {
    page.iter()
        .filter(|stored| {
            !stored.from_me
                && matches!(
                    stored.status,
                    oxidezap_chat_store::MessageStatus::Read
                        | oxidezap_chat_store::MessageStatus::Played
                )
        })
        .map(|stored| stored.id.clone())
}

/// Mark the watched updates read, in the broadcast and nowhere else.
///
/// Our own updates are left alone: they are never unseen to begin with, and a
/// row from us carries the peer-read ticks in `is_read`, which a local view has
/// no business setting.
fn apply_status_views(chats: &mut [oxidezap_core::Chat], watched: &HashSet<String>) {
    if watched.is_empty() {
        return;
    }
    for chat in chats.iter_mut().filter(|chat| chat.is_status) {
        for message in &mut chat.messages {
            if !message.is_from_me && watched.contains(&message.id) {
                message.is_read = true;
            }
        }
    }
}

impl WhatsAppClient {
    const HISTORY_CHAT_LIMIT: i64 = 100;
    /// One page of a conversation, for a front end that asked for one.
    ///
    /// The number WhatsApp Web's own on-demand history request uses
    /// (`history_sync_on_demand_message_count`), and near enough to a screenful
    /// of bubbles that scrolling back asks again rather than stalling.
    pub const MESSAGE_PAGE: i64 = 50;
    /// One page of the chat list.
    ///
    /// WA Web's `web_init_chat_batch_size`, and the same number the list has
    /// always loaded at once.
    pub const CHAT_PAGE: i64 = 100;
    /// How many of a chat's newest messages the attach load carries.
    ///
    /// Not a timeline — a front end asks for that when it has somewhere to
    /// draw it. What stays is what this side needs to do its own job: the
    /// newest row, which the chat list draws its preview from, and the unread
    /// tail, which is the set of receipts a read owes and the second a read is
    /// bounded by. A chat nobody has an unread message in needs almost
    /// nothing; the floor is there so an ordinary same-second burst is
    /// covered rather than truncated.
    const ATTACH_FLOOR: i64 = 8;
    /// And no more than a page, however many are unread: past that the front
    /// end is asking for history anyway.
    const ATTACH_CEILING: i64 = 50;
    /// Quiet window before reloading: one history-sync chunk commits as many
    /// write batches, each emitting a change; reload once per burst.
    const RELOAD_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

    /// One task for the whole session: chat-store invalidations -> debounced
    /// load_history -> HistoryLoaded.
    ///
    /// Exits when the session does, which is `stopping` and not the store: it
    /// holds an `Arc<ChatStore>` itself, and that store owns the sender its
    /// receiver is waiting on — so "the store went away" is a thing this task
    /// makes impossible by existing. On a desktop that never showed, because
    /// dropping the runtime took the task with it; on a page nothing does.
    fn spawn_history_reloader(
        mut changes: tokio::sync::broadcast::Receiver<oxidezap_chat_store::StoreChange>,
        chat_store: Arc<ChatStore>,
        bot: &Bot,
        ui_tx: &mpsc::UnboundedSender<UiEvent>,
        reload: Arc<tokio::sync::Notify>,
        names: Arc<NameBook>,
        mut stopping: tokio::sync::watch::Receiver<()>,
    ) {
        use tokio::sync::broadcast::error::RecvError;

        let client = bot.client();
        let ui_tx = ui_tx.clone();
        crate::exec::spawn_owned(async move {
            let mut open = true;
            while open {
                let mut scope = ReloadScope::empty();
                // Either a store change or somebody asking outright. An
                // explicit ask widens to everything, because the asker is a
                // front end that has just attached and holds nothing.
                let mut asked = false;
                tokio::select! {
                    change = changes.recv() => match change {
                        Ok(change) => scope.widen(Some(&change)),
                        Err(RecvError::Lagged(_)) => scope.widen(None),
                        Err(RecvError::Closed) => break,
                    },
                    () = reload.notified() => {
                        scope.widen(None);
                        asked = true;
                    }
                    // Without a final load: the session is going, and a
                    // reload would read a store that is about to be deleted
                    // and publish it at a front end that has already left.
                    _ = stopping.changed() => break,
                }
                // Drain the burst; a quiet window flushes the reload.
                //
                // Not entered, and broken out of, when somebody asks outright.
                // The debounce is there to fold a history sync's many
                // committed batches into one load, and the cost of folding is
                // a fifth of a second before the first query runs. A front end
                // that has just attached is holding nothing and is the one
                // caller that waits on this — and there is nothing to
                // coalesce for it, because it asked for everything.
                //
                // The ask has to be watched *here* as well as in the select
                // above, not only skipped when it happened to win it: during a
                // history sync the changes never stop arriving, so a drain
                // that waits on them alone has no quiet window to end on and
                // the asker waits out the whole sync.
                while !asked {
                    tokio::select! {
                        _ = stopping.changed() => return,
                        change = crate::exec::with_timeout(changes.recv(), Self::RELOAD_DEBOUNCE) => {
                            match change {
                                Some(Ok(change)) => scope.widen(Some(&change)),
                                Some(Err(RecvError::Lagged(_))) => scope.widen(None),
                                Some(Err(RecvError::Closed)) => {
                                    // Reload once more: these changes were committed.
                                    open = false;
                                    break;
                                }
                                // The quiet window: flush what has piled up.
                                None => break,
                            }
                        }
                        () = reload.notified() => {
                            scope.widen(None);
                            asked = true;
                        }
                    }
                }
                // An empty COMPLETE load still goes out: the UI prunes
                // against the loaded set, so deleting/archiving the last chat
                // elsewhere must clear the list here too. An empty narrowed
                // one names nothing the list shows (an archived chat, or one
                // past the window) and has nothing to say.
                match Self::load_history_scoped(&chat_store, &client, scope.chats(), &names).await {
                    Ok(loaded) if loaded.chats.is_empty() && !loaded.complete => {}
                    Ok(loaded) => {
                        if ui_tx.send(loaded.into_event()).is_err() {
                            break;
                        }
                    }
                    Err(e) => warn!("failed to reload history after store change: {e}"),
                }
            }
        });
    }

    /// Build the UI chat list from the durable store: chats in display order,
    /// each with its most recent page of messages. Media bodies are not
    /// hydrated here (the proto is in the store; download stays on demand).
    /// The returned flag says whether this is the store's WHOLE display list;
    /// it comes from the raw entry count, since PN/LID collapsing can shrink
    /// a truncated fetch back under the limit.
    async fn load_history(
        chat_store: &Arc<ChatStore>,
        client: &Arc<Client>,
        names: &NameBook,
    ) -> Result<LoadedHistory, oxidezap_chat_store::ChatStoreError> {
        Self::load_history_scoped(chat_store, client, None, names).await
    }

    /// [`load_history`](Self::load_history), restricted to the chats `only`
    /// names when it names any.
    ///
    /// The whole-list rebuild is what every invalidation used to cost: one
    /// message page, its reactions and its sender names per chat, for all of
    /// them, and receipts alone fire it several times per sent message. A
    /// receipt or an ack moves rows inside one conversation and leaves the
    /// list's order, membership and names exactly as they were, so the load
    /// it triggers can be that conversation's.
    async fn load_history_scoped(
        chat_store: &Arc<ChatStore>,
        client: &Arc<Client>,
        only: Option<&HashSet<String>>,
        names: &NameBook,
    ) -> Result<LoadedHistory, oxidezap_chat_store::ChatStoreError> {
        // A whole-list load is the pass that re-reads the address book, so it
        // is the one that drops what the book remembers: a contact renamed on
        // the phone appears under its new name without a restart, and the
        // scoped loads in between — which run per receipt — still pay nothing.
        if only.is_none() {
            names.forget();
        }
        let mut entries = chat_store.chats(false, Self::HISTORY_CHAT_LIMIT).await?;
        // A narrowed load says nothing about the chats it left out, so it is
        // never the whole display list and must never drive the UI's prune.
        let complete = only.is_none() && (entries.len() as i64) < Self::HISTORY_CHAT_LIMIT;
        // Where this load stopped, so the front end's first "load more" is a
        // page it does not have rather than the page it was just handed. Taken
        // from the raw entries, before the alias filter below and before the
        // PN/LID collapse: a cursor is a position in the store's own order,
        // and both of those change what the list looks like without moving
        // that position. A complete load has nothing after it, and a narrowed
        // one is not a position in the list at all.
        let next = (only.is_none() && !complete)
            .then(|| entries.last().map(chat_cursor))
            .flatten();
        if let Some(only) = only {
            let wanted = Self::alias_closure(client, &entries, only, names).await;
            entries.retain(|entry| wanted.contains(&entry.jid.to_non_ad_string()));
            // The page above is the hundred most recently active chats, and a
            // narrowed load is about the chats somebody named — which is not
            // the same set. A chat that has fallen past that window would be
            // filtered down to nothing here, and a load with nothing in it
            // publishes nothing: the invalidation that asked for it would be
            // silently spent, leaving every front end on rows that changed.
            // Asked for by name instead, and only for what the page missed.
            let found: HashSet<String> = entries
                .iter()
                .map(|entry| entry.jid.to_non_ad_string())
                .collect();
            for jid in only.iter().filter(|jid| !found.contains(*jid)) {
                let Ok(parsed) = jid.parse::<Jid>() else {
                    continue;
                };
                match chat_store.chat(&parsed).await {
                    Ok(Some(entry)) => entries.push(entry),
                    // No row: the chat is live-only, or gone. Either way this
                    // load has nothing to say about it, which is what a
                    // narrowed load is allowed to be.
                    Ok(None) => {}
                    Err(e) => warn!(
                        "failed to look up {} for a scoped load: {e}",
                        observe_str(jid)
                    ),
                }
            }
        }
        // The other half of every row this load carries. A PN/LID pair is one
        // conversation and the collapse below is what makes its unread count
        // the pair's sum — but only over the rows it is given, and the window
        // above ends wherever the store's order puts it. Half a pair alone is
        // a chat with half the pair's unread count, and now that a front end
        // continues *past* this window rather than re-fetching it, nothing
        // else would go back for the other half. The cursor is already taken
        // from the raw boundary, so this cannot move where the list continues.
        let entries = if only.is_none() {
            Self::with_alias_rows(chat_store, client, names, entries).await
        } else {
            // A narrowed load has its own closure, which starts from the
            // chats somebody named rather than from a page.
            entries
        };
        let chats =
            Self::hydrate_entries(chat_store, client, names, entries, Self::attach_page).await?;
        Ok(LoadedHistory {
            chats,
            complete,
            next,
        })
    }

    /// Turn store rows into the chats a front end draws.
    ///
    /// The shared half of every read that produces chats: one page per chat
    /// in one read, the PN/LID collapse, reactions, sender names, the unread
    /// tail and the preview. `page_for` says how many messages each chat's
    /// page carries, because the two callers want different amounts — an
    /// attach carries what this side needs of a chat, a list page carries the
    /// row and nothing else.
    async fn hydrate_entries(
        chat_store: &Arc<ChatStore>,
        client: &Arc<Client>,
        names: &NameBook,
        entries: Vec<ChatEntry>,
        page_for: impl Fn(&ChatEntry) -> i64,
    ) -> Result<Vec<oxidezap_core::Chat>, oxidezap_chat_store::ChatStoreError> {
        // Every chat's page in one read, before the loop that needs them: the
        // per-chat call is a permit, a blocking task and a transaction each,
        // and an attaching front end asks for a hundred of them at once.
        // Sized per chat by what this side needs of it — see `attach_page`.
        let mut pages = chat_store
            .pages(
                entries
                    .iter()
                    .map(|entry| (entry.jid.clone(), page_for(entry)))
                    .collect(),
            )
            .await?;
        let mut chats: Vec<oxidezap_core::Chat> = Vec::with_capacity(entries.len());
        // Updates whose stored ack says they were watched here. Gathered from
        // the rows as they are read and applied once at the end; see the call
        // to `apply_status_views` below for why it cannot be done in place.
        let mut status_views: HashSet<String> = HashSet::new();
        for entry in entries {
            // Same PN->LID mapping live events go through, or the restored
            // chat and the next live message split into two conversations.
            // A PN/LID pair of stored rows collapses into one chat: the most
            // recently active row (entries arrive in display order) keeps the
            // metadata, the older row's messages merge in.
            let identity = names.identity(client, &entry.jid).await;
            let (name, name_priority) = names
                .resolve(chat_store, &entry.jid, entry.name.as_deref(), &identity)
                .await;
            let jid_str = identity.canonical_jid.clone();
            if let Some(existing) = chats.iter_mut().find(|c| c.jid == jid_str) {
                let mut page = pages.remove(&entry.jid.to_string()).unwrap_or_default();
                page.reverse();
                if existing.is_status {
                    status_views.extend(watched_ids(&page));
                }
                let mut msgs: Vec<ChatMessage> =
                    page.into_iter().map(stored_to_chat_message).collect();
                Self::hydrate_reactions(chat_store, client, names, &entry.jid, &mut msgs).await;
                Self::canonicalize_quoted_authors(client, names, &mut msgs).await;
                // Groups *and* the status broadcast: both carry rows written
                // by many people, and a hydrated row has no push name on it.
                if existing.is_group || existing.is_status {
                    Self::hydrate_sender_names(
                        chat_store,
                        client,
                        &mut msgs,
                        names,
                        existing.is_status,
                    )
                    .await;
                }
                // Each alias still needs its unread tail marked for receipts,
                // but PN/LID counters describe the same logical chat.
                mark_unread_tail(&mut msgs, entry.unread_count.max(0) as u32);
                merge_alias_history_messages(existing, msgs, entry.unread_count.max(0) as u32);
                // A page is assigned rather than added a row at a time, so
                // the naming `add_message` does per row has to be run over it.
                existing.name_quoted_authors();
                existing.manually_unread |= entry.unread_count < 0;
                existing.set_name_if_better(name, name_priority);
                continue;
            }
            // Store-originated: the HistoryLoaded prune may drop it when a
            // later complete load no longer returns it.
            let mut chat = oxidezap_core::Chat::from_store(jid_str.clone(), name, name_priority);
            chat.unread_count = entry.unread_count.max(0) as u32;
            // -1 = manually marked unread (WA Web convention); .max(0) above
            // must not silently eat the flag.
            chat.manually_unread = entry.unread_count < 0;
            chat.last_message_time = entry.last_message_at;

            let mut page = pages.remove(&entry.jid.to_string()).unwrap_or_default();
            page.reverse(); // store returns newest-first; the UI renders oldest-first
            if chat.is_status {
                status_views.extend(watched_ids(&page));
            }
            chat.messages = page.into_iter().map(stored_to_chat_message).collect();
            Self::hydrate_reactions(chat_store, client, names, &entry.jid, &mut chat.messages)
                .await;
            Self::canonicalize_quoted_authors(client, names, &mut chat.messages).await;
            if chat.is_group || chat.is_status {
                let is_status = chat.is_status;
                Self::hydrate_sender_names(
                    chat_store,
                    client,
                    &mut chat.messages,
                    names,
                    is_status,
                )
                .await;
            }
            mark_unread_tail(&mut chat.messages, chat.unread_count);
            // After the sender names, because the best answer for "who wrote
            // the message this is replying to" is usually the reply's own
            // neighbour, and it has only just been named.
            chat.name_quoted_authors();
            chat.last_message =
                history_preview(entry.last_message_preview.clone(), chat.messages.last());
            chats.push(chat);
        }
        // The views come off the rows that were just read, not from a second
        // query: a watched update is one whose stored ack reached `Read`.
        // Applied in one pass at the end rather than inside the branches
        // above, because the alias merge re-marks rows unread from the chat
        // it merges into and would undo a fix applied before it.
        apply_status_views(&mut chats, &status_views);
        Ok(chats)
    }

    /// How many of one chat's newest messages the attach load carries.
    ///
    /// The unread tail, because those are the receipts a read owes and the
    /// second it is bounded by, with a floor that covers a same-second burst
    /// and the newest row the list previews from. The status broadcast is the
    /// exception: its feed *is* those rows — there is no conversation to open
    /// that would ask for more — so it keeps a whole page.
    /// A page of chats, with each row's other half beside it.
    ///
    /// A PN/LID pair is one conversation and `hydrate_entries` is what
    /// collapses it — but only over the rows it is given, and a page boundary
    /// falls wherever the store's order puts it. Half a pair alone hydrates
    /// into a chat carrying half the pair's unread count, which the window
    /// merges over the whole one it already had.
    ///
    /// Both halves are pulled in, from whichever half the page holds, so the
    /// answer is the same collapsed chat either way: a page that lands after
    /// one that already carried this person repeats it rather than reducing
    /// it. Costs one read per row that has an alias the page does not.
    async fn with_alias_rows(
        chat_store: &Arc<ChatStore>,
        client: &Arc<Client>,
        names: &NameBook,
        entries: Vec<ChatEntry>,
    ) -> Vec<ChatEntry> {
        let mut have: HashSet<String> = entries.iter().map(|e| e.jid.to_string()).collect();
        let mut wanted: Vec<Jid> = Vec::new();
        for entry in &entries {
            let identity = names.identity(client, &entry.jid).await;
            for alias in &identity.contact_jids {
                // `have` is what this page holds plus what has already been
                // asked for, so a pair whose halves are both on the page
                // costs nothing and neither half is asked for twice.
                if have.insert(alias.to_string()) {
                    wanted.push(alias.clone());
                }
            }
        }
        let mut entries = entries;
        if !wanted.is_empty() {
            // One read for the page, not one per alias: most people have a
            // single row, and finding that out a hundred times over is a
            // hundred permits and transactions spent on nothing.
            match chat_store.chats_by_jids(wanted).await {
                Ok(rows) => entries.extend(rows),
                Err(e) => log::warn!("could not read the aliases of a page of chats: {e}"),
            }
        }
        // Display order again, because that is what decides which half of a
        // pair keeps the metadata — the rows appended above are behind the
        // page's own until they are put back in it.
        entries.sort_by(|a, b| {
            let key = |e: &ChatEntry| {
                (
                    e.pinned_at.map(|t| t.timestamp_millis()),
                    e.last_message_at.map(|t| t.timestamp_millis()),
                )
            };
            key(b)
                .cmp(&key(a))
                .then_with(|| b.jid.to_string().cmp(&a.jid.to_string()))
        });
        entries
    }

    fn attach_page(entry: &ChatEntry) -> i64 {
        if is_status_broadcast(entry) {
            return Self::MESSAGE_PAGE;
        }
        i64::from(entry.unread_count.max(0)).clamp(Self::ATTACH_FLOOR, Self::ATTACH_CEILING)
    }

    /// Every storage key the invalidated chats are held under.
    ///
    /// A PN/LID pair collapses into one chat on load, and the collapse is what
    /// makes its unread counter the pair's sum: rebuilding one half alone
    /// would not be a smaller answer but a wrong one. The expansion runs off
    /// the entries the invalidated keys match, so it costs a mapping lookup
    /// per named chat rather than one per chat in the list.
    async fn alias_closure(
        client: &Arc<Client>,
        entries: &[ChatEntry],
        only: &HashSet<String>,
        names: &NameBook,
    ) -> HashSet<String> {
        let mut wanted = only.clone();
        for entry in entries {
            if !only.contains(&entry.jid.to_non_ad_string()) {
                continue;
            }
            let identity = names.identity(client, &entry.jid).await;
            wanted.insert(identity.canonical_jid.clone());
            wanted.extend(identity.contact_jids.iter().map(Jid::to_string));
        }
        wanted
    }

    /// Reactions live in their own table, so hydrated messages come out with
    /// an empty map; fold the stored rows back in. Per-message point lookups:
    /// the store exposes no per-chat batch query. Best-effort: one bad row
    /// must not abort the whole history load and blank the chat list.
    async fn hydrate_reactions(
        chat_store: &Arc<ChatStore>,
        client: &Arc<Client>,
        names: &NameBook,
        chat_jid: &Jid,
        msgs: &mut [ChatMessage],
    ) {
        // One query for the page. A message with no reactions is the common
        // case by a wide margin, and asking per message spent a pooled read on
        // each of them.
        let ids: Vec<String> = msgs.iter().map(|msg| msg.id.clone()).collect();
        let mut by_message = match chat_store.reactions_for(chat_jid, ids).await {
            Ok(found) => found,
            Err(e) => {
                warn!(
                    "failed to hydrate reactions for {}: {e}",
                    observe_str(&chat_jid.to_string())
                );
                return;
            }
        };
        for msg in msgs.iter_mut() {
            let Some(entries) = by_message.remove(&msg.id) else {
                continue;
            };
            // The store keeps one row per sender, and the live path publishes
            // reactors under their canonical JID — so a row stored under one
            // alias has to be read back under the same name, or a later
            // replacement or removal cannot find it and the two aliases stand
            // as two people. Coalesced here as well as renamed: two rows *are*
            // two rows in the table, and the answer is the same one the live
            // path gives, which is that the newest wins. Rows arrive oldest
            // first, so the last write is it.
            //
            // A linear scan rather than a map: a message has a handful of
            // reactors, and keeping the order they were stored in is what
            // makes a reloaded row draw them the way the live one did.
            let mut latest: Vec<(String, String)> = Vec::new();
            for entry in entries {
                let who = names.identity(client, &entry.sender_jid).await;
                match latest.iter_mut().find(|(jid, _)| *jid == who.canonical_jid) {
                    Some((_, emoji)) => *emoji = entry.emoji,
                    None => latest.push((who.canonical_jid.clone(), entry.emoji)),
                }
            }
            // Through `add_reaction` rather than into the map, because the
            // bounds on a message's reactions live there: writing the rows
            // straight in restored every stored reactor, so a message the
            // live path had capped came back over the cap after a reload —
            // and drew a different set from the copy beside it.
            for (sender, emoji) in latest {
                msg.add_reaction(emoji, sender);
            }
        }
    }

    /// Group bubbles label their sender, but a hydrated row carries no push
    /// name; the book answers from the same order the live path uses, so a
    /// reloaded bubble and the one that arrived a moment ago agree.
    ///
    /// A group page names the same handful of people over and over and the
    /// book memoizes per JID, so a page costs one lookup per unique sender
    /// rather than one per row.
    /// File a quote's author under the identity their own bubbles are filed
    /// under.
    ///
    /// Every other sender field on a message goes through
    /// `identity.canonical_jid`; the one on a quote came straight off the
    /// envelope, which is a phone number where the chat is keyed by a LID and
    /// carries the sending device's suffix besides. `Chat::quoted_author`
    /// looks a participant up by exact string, so the bar above a reply read
    /// "Unknown contact" — or a bare number — over bubbles from the same
    /// person, named from the address book, an inch above it.
    async fn canonicalize_quoted_authors(
        client: &Arc<Client>,
        names: &NameBook,
        msgs: &mut [ChatMessage],
    ) {
        for msg in msgs.iter_mut() {
            let Some(quoted) = msg.quoted.as_mut() else {
                continue;
            };
            let Ok(jid) = quoted.sender.parse::<Jid>() else {
                continue;
            };
            quoted
                .sender
                .clone_from(&names.identity(client, &jid).await.canonical_jid);
        }
    }

    async fn hydrate_sender_names(
        chat_store: &Arc<ChatStore>,
        client: &Arc<Client>,
        msgs: &mut [ChatMessage],
        names: &NameBook,
        is_status: bool,
    ) {
        for msg in msgs.iter_mut() {
            if msg.is_from_me || msg.sender_name.is_some() {
                continue;
            }
            let Ok(jid) = msg.sender.parse::<Jid>() else {
                continue;
            };
            let identity = names.identity(client, &jid).await;
            // One person, one row in the feed. The status broadcast is
            // grouped by sender, and the same contact reaches it under a
            // phone number on some updates and their LID on others — which
            // split their ring, their unseen count and their playback run in
            // two. Chat identities are canonicalized on the way in; these had
            // been left as they arrived.
            if is_status {
                msg.sender.clone_from(&identity.canonical_jid);
            }
            // The same answer the live path gives, and for the same reason a
            // number is not one: this field only ever gains a value, because
            // `Chat::update_participant` fills blanks. A row stamped with a
            // phone number could never be renamed by the push name that
            // arrives a second later, and the same person would read as a
            // number on their reloaded bubbles and by name on their new ones.
            // Drawing a number where nothing is known is the *renderer's* job.
            msg.sender_name = match names.resolve(chat_store, &jid, None, &identity).await {
                (_, crate::names::priority::NONE) => None,
                (name, _) => Some(name),
            };
        }
    }
}

/// The chat-list preview for a hydrated chat.
///
/// The store's `last_message_preview` is the newest message's TEXT, and plenty
/// of messages have none: a photo or a voice note without a caption, a revoked
/// message whose content was tombstoned. The bubble still has a label — the
/// same one the live path puts in the list — so it answers where the column
/// cannot, and a chat that plainly has messages stops rendering as "No
/// messages".
fn history_preview(stored: Option<String>, newest: Option<&ChatMessage>) -> Option<String> {
    stored.or_else(|| newest.map(ChatMessage::preview_text))
}

/// Convert a durable store row into the UI message model. Media stays
/// download-on-demand (the encoded proto lives in the store if needed later).
/// Media metadata (thumbnail + download info) from a message proto, without
/// downloading anything. Shared by hydration; the live path additionally
/// downloads images/stickers eagerly.
/// The stand-in bytes a still carries before its media is fetched.
struct Still {
    data: Vec<u8>,
    mime: String,
    is_preview: bool,
}

fn thumbnail_bytes(thumbnail: Option<&[u8]>) -> Vec<u8> {
    thumbnail
        .filter(|t| !t.is_empty())
        .unwrap_or_default()
        .to_vec()
}

/// What a still is holding, decided once for the live path and the hydrated
/// one. They had drifted: the live path flagged a thumbnail as a preview with
/// no download metadata to make good on it, so the viewer refused to open the
/// only bytes that will ever exist and the daemon refused to cache them.
///
/// A preview is bytes standing in for a fetch that can actually be made, and
/// the mime describes what is in hand rather than what is being waited for.
/// The video paths do not come through here on purpose: a poster frame is
/// never the video, download metadata or not.
fn still_preview(
    thumbnail: Vec<u8>,
    thumbnail_mime: &str,
    own_mime: String,
    downloadable: bool,
) -> Still {
    let has_preview = !thumbnail.is_empty();
    Still {
        mime: if has_preview {
            thumbnail_mime.to_string()
        } else {
            own_mime
        },
        is_preview: has_preview && downloadable,
        data: thumbnail,
    }
}

fn media_metadata(msg: &wa::Message) -> Option<MediaContent> {
    if let Some(sticker) = effective_sticker(msg) {
        let mime = sticker
            .mimetype
            .clone()
            .unwrap_or_else(|| "image/webp".to_string());
        let downloadable = DownloadableBuilder {
            direct_path: sticker.direct_path.as_deref(),
            media_key: sticker.media_key.as_deref(),
            file_enc_sha256: sticker.file_enc_sha256.as_deref(),
            file_length: sticker.file_length,
            mime_type: &mime,
            duration_secs: None,
            download_type: DownloadMediaType::Sticker,
        }
        .build();
        let still = still_preview(
            thumbnail_bytes(sticker.png_thumbnail.as_deref()),
            "image/png",
            mime,
            downloadable.is_some(),
        );
        if still.data.is_empty() && downloadable.is_none() {
            return None;
        }
        return Some(MediaContent {
            media_type: MediaType::Sticker,
            data: Arc::new(still.data),
            cache_key: None,
            mime_type: still.mime,
            width: sticker.width,
            height: sticker.height,
            caption: None,
            file_name: None,
            data_is_preview: still.is_preview,
            waveform: None,
            downloadable,
            // What the sticker *is*, not what the stand-in bytes are: the
            // preview is a still, but the flag describes the file that
            // replaces it, and `data_is_preview` beside it already says which
            // of the two is in hand.
            is_animated: sticker.is_animated.unwrap_or(false),
            duration_secs: None,
        });
    }
    if let Some(image) = msg.image_message.as_option() {
        let downloadable = DownloadableBuilder {
            direct_path: image.direct_path.as_deref(),
            media_key: image.media_key.as_deref(),
            file_enc_sha256: image.file_enc_sha256.as_deref(),
            file_length: image.file_length,
            mime_type: image.mimetype.as_deref().unwrap_or("image/jpeg"),
            duration_secs: None,
            download_type: DownloadMediaType::Image,
        }
        .build();
        let still = still_preview(
            thumbnail_bytes(image.jpeg_thumbnail.as_deref()),
            "image/jpeg",
            image
                .mimetype
                .clone()
                .unwrap_or_else(|| "image/jpeg".to_string()),
            downloadable.is_some(),
        );
        if still.data.is_empty() && downloadable.is_none() {
            return None;
        }
        return Some(MediaContent {
            media_type: MediaType::Image,
            data: Arc::new(still.data),
            cache_key: None,
            mime_type: still.mime,
            width: image.width,
            height: image.height,
            caption: image.caption.clone(),
            file_name: None,
            downloadable,
            is_animated: false,
            duration_secs: None,
            data_is_preview: still.is_preview,
            waveform: None,
        });
    }
    // PTVs (round video notes) are VideoMessage in a different field.
    if let Some(video) = msg
        .ptv_message
        .as_option()
        .or(msg.video_message.as_option())
    {
        let downloadable = DownloadableBuilder {
            direct_path: video.direct_path.as_deref(),
            media_key: video.media_key.as_deref(),
            file_enc_sha256: video.file_enc_sha256.as_deref(),
            file_length: video.file_length,
            mime_type: video.mimetype.as_deref().unwrap_or("video/mp4"),
            duration_secs: video.seconds,
            download_type: DownloadMediaType::Video,
        }
        .build();
        let thumbnail = video
            .jpeg_thumbnail
            .as_ref()
            .filter(|t| !t.is_empty())
            .cloned()
            .unwrap_or_default();
        if thumbnail.is_empty() && downloadable.is_none() {
            return None;
        }
        // The same as the live path: a poster frame, not the video.
        let data_is_preview = !thumbnail.is_empty();
        return Some(MediaContent {
            media_type: MediaType::Video,
            data: Arc::new(thumbnail),
            cache_key: None,
            mime_type: "image/jpeg".to_string(),
            width: video.width,
            height: video.height,
            caption: video.caption.clone(),
            file_name: None,
            downloadable,
            is_animated: false,
            duration_secs: video.seconds,
            data_is_preview,
            waveform: None,
        });
    }
    if let Some(audio) = msg.audio_message.as_option() {
        let mime = audio
            .mimetype
            .clone()
            .unwrap_or_else(|| "audio/ogg; codecs=opus".to_string());
        let downloadable = DownloadableBuilder {
            direct_path: audio.direct_path.as_deref(),
            media_key: audio.media_key.as_deref(),
            file_enc_sha256: audio.file_enc_sha256.as_deref(),
            file_length: audio.file_length,
            mime_type: &mime,
            duration_secs: audio.seconds,
            download_type: DownloadMediaType::Audio,
        }
        .build()?;
        return Some(MediaContent {
            media_type: MediaType::Audio,
            data: Arc::new(vec![]),
            cache_key: None,
            mime_type: mime.clone(),
            width: None,
            height: None,
            caption: None,
            file_name: None,
            downloadable: Some(downloadable),
            is_animated: false,
            duration_secs: audio.seconds,
            data_is_preview: false,
            // The same field the live path reads. Dropping it here made every
            // voice note flatten to a placeholder shape the moment history
            // was reloaded — which is most of the time.
            waveform: audio
                .waveform
                .as_deref()
                .filter(|w| !w.is_empty())
                .map(|w| Arc::new(w.to_vec())),
        });
    }
    if let Some(doc) = msg.document_message.as_option() {
        let mime = doc.mimetype.clone().unwrap_or_default();
        let downloadable = DownloadableBuilder {
            direct_path: doc.direct_path.as_deref(),
            media_key: doc.media_key.as_deref(),
            file_enc_sha256: doc.file_enc_sha256.as_deref(),
            file_length: doc.file_length,
            mime_type: &mime,
            duration_secs: None,
            download_type: DownloadMediaType::Document,
        }
        .build();
        return Some(MediaContent {
            media_type: MediaType::Document,
            data: Arc::new(vec![]),
            cache_key: None,
            mime_type: mime,
            width: None,
            height: None,
            caption: doc.caption.clone(),
            file_name: doc.file_name.clone(),
            downloadable,
            is_animated: false,
            duration_secs: None,
            data_is_preview: false,
            waveform: None,
        });
    }
    None
}

/// Some animated stickers arrive wrapped in the `lottie_sticker_message`
/// future-proof envelope instead of the top-level `sticker_message`.
fn effective_sticker(msg: &wa::Message) -> Option<&wa::message::StickerMessage> {
    msg.sticker_message.as_option().or_else(|| {
        msg.lottie_sticker_message
            .as_option()
            .and_then(|w| w.message.as_option())
            .and_then(|m| m.sticker_message.as_option())
    })
}

/// How many lanes events about a subject are spread across.
///
/// Fixed rather than one per subject: a lane is a task and a queue, and a
/// lane per chat is one of each for every conversation an account has ever
/// had. Subjects share a lane by hash, so two busy chats can queue behind
/// each other — which costs latency, where the alternative costs order.
const EVENT_LANES: usize = 8;

/// Events about one subject, handled in the order they arrived.
///
/// The event stream reaches this side already ordered, and handling each
/// event on its own task threw that away: a `CallEndedElsewhere` could run
/// before the `IncomingCall` it ends, leaving a card ringing for a call that
/// is over, and a receipt could run before the message it answers. Ordering
/// only matters between events about the same thing, so events are keyed by
/// their call or their chat and a key always reaches the same lane. Anything
/// naming neither is session-wide and gets a lane of its own, so a pairing
/// code never waits behind a conversation.
struct EventLanes {
    lanes: Vec<mpsc::UnboundedSender<Arc<Event>>>,
}

impl EventLanes {
    fn new<F, Fut>(handle: F, stopping: tokio::sync::watch::Receiver<()>) -> Self
    where
        F: Fn(Arc<Event>) -> Fut + Clone + crate::exec::MaybeSend + 'static,
        Fut: Future<Output = ()> + crate::exec::MaybeSend + 'static,
    {
        let lanes = (0..=EVENT_LANES)
            .map(|_| {
                let (tx, mut rx) = mpsc::unbounded_channel::<Arc<Event>>();
                let handle = handle.clone();
                let mut stopping = stopping.clone();
                crate::exec::spawn_owned(async move {
                    loop {
                        let event = tokio::select! {
                            event = rx.recv() => match event {
                                Some(event) => event,
                                None => return,
                            },
                            // Dropping the senders is not enough on its own:
                            // a receiver hands out everything already queued
                            // before it answers `None`, so a lane would work
                            // through a backlog belonging to an account this
                            // session no longer speaks for. On a page that
                            // matters twice over, where nothing cancels a
                            // spawned task and the backlog keeps the old
                            // client and its store alive.
                            _ = stopping.changed() => return,
                        };
                        handle(event).await;
                    }
                });
                tx
            })
            .collect();
        Self { lanes }
    }

    async fn dispatch(&mut self, client: &Client, event: Arc<Event>) {
        // A batch may span chats, and a lane is one chat's order: sent whole
        // on the first message's lane, a receipt for a later chat in it runs
        // on that chat's own lane and can overtake the message it answers.
        // Split, each chat's messages keep their order against everything
        // else about that chat, and two chats in one batch were never ordered
        // against each other.
        for event in split_by_subject(&event) {
            let lane = lane_for(client, &event).await;
            let _ = self.lanes[lane].send(event);
        }
    }
}

/// One event per subject it is about, which for everything but a batch of
/// messages is the event itself.
fn split_by_subject(event: &Arc<Event>) -> Vec<Arc<Event>> {
    let Event::Messages(batch) = &**event else {
        return vec![Arc::clone(event)];
    };
    let mut chats: Vec<String> = Vec::new();
    for inbound in batch.iter() {
        let chat = inbound.info.source.chat.to_string();
        if !chats.contains(&chat) {
            chats.push(chat);
        }
    }
    if chats.len() <= 1 {
        return vec![Arc::clone(event)];
    }
    chats
        .into_iter()
        .map(|chat| {
            let messages: Arc<[whatsapp_rust::wacore::types::events::InboundMessage]> = batch
                .iter()
                .filter(|inbound| inbound.info.source.chat.to_string() == chat)
                .cloned()
                .collect();
            // The origin travels with every part: it says how the batch was
            // delivered, which is as true of one chat's share of it as of the
            // whole, and it is what decides whether media is fetched eagerly.
            Arc::new(Event::Messages(
                whatsapp_rust::wacore::types::events::MessageBatch::builder()
                    .messages(messages)
                    .origin(batch.origin)
                    .build(),
            ))
        })
        .collect()
}

/// Which lane an event is handled on. Same subject, same lane.
///
/// The address is canonicalized first, which is the whole reason this is not
/// a pure function of the event. The wire names one peer two ways and the two
/// hash to different lanes, so a message under a phone number and its receipt
/// under the LID were handled concurrently: the receipt could overtake the
/// message it answers -- most easily while that message waits on an eager
/// media fetch -- and a front end drops a receipt naming a row it has not
/// been given yet. The library keeps the pairing in memory in front of its
/// store, so this is a map read for a peer already seen and not asked at all
/// of a LID.
async fn lane_for(client: &Client, event: &Event) -> usize {
    let subject = match event_subject(event) {
        Some(Subject::Call(id)) => Some(id),
        Some(Subject::Chat(jid)) => Some(normalize_chat_jid(client, &jid.to_string()).await),
        None => None,
    };
    lane_of(subject.as_deref())
}

/// The lane a subject hashes to, or the session-wide one for no subject.
fn lane_of(subject: Option<&str>) -> usize {
    match subject {
        Some(subject) => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&subject, &mut hasher);
            (std::hash::Hasher::finish(&hasher) as usize) % EVENT_LANES
        }
        None => EVENT_LANES,
    }
}

/// What an event is about, for the lane that keeps its order.
///
/// A call or a chat. `None` is a session-wide event, which is about the
/// account rather than about anything in it.
enum Subject {
    Call(String),
    Chat(Jid),
}

fn event_subject(event: &Event) -> Option<Subject> {
    match event {
        Event::IncomingCall(call) => Some(Subject::Call(call.action.call_id().to_string())),
        Event::MissedCall(missed) => Some(Subject::Call(missed.call_id.clone())),
        Event::CallEndedElsewhere(ended) => Some(Subject::Call(ended.call_id.clone())),
        Event::Messages(batch) => batch
            .iter()
            .next()
            .map(|inbound| Subject::Chat(inbound.info.source.chat.clone())),
        Event::Receipt(receipt) => Some(Subject::Chat(receipt.source.chat.clone())),
        Event::ChatPresence(update) => Some(Subject::Chat(update.source.chat.clone())),
        // Both name somebody, and both handlers go to the store for a name or
        // an identity. On the session-wide lane a burst of either delayed
        // `Connected`, `PairingQrCode` and `LoggedOut` behind it, which are
        // the events a window is waiting on to draw anything at all.
        Event::Presence(update) => Some(Subject::Chat(update.from.clone())),
        Event::GroupUpdate(update) => Some(Subject::Chat(update.group_jid.clone())),
        _ => None,
    }
}

impl Subject {
    /// The address, before canonicalization. For tests and for logging: a
    /// lane is chosen from the canonical form, which needs the client.
    #[cfg(test)]
    fn as_written(&self) -> String {
        match self {
            Self::Call(id) => id.clone(),
            Self::Chat(jid) => jid.to_string(),
        }
    }
}

/// Un-read the newest `unread` incoming rows of a hydrated page.
///
/// [`stored_to_chat_message`] reads an incoming row back as read, because the
/// store keeps read state on the chat's counter and not on the row — so every
/// caller that hydrates stored rows owes this correction. Skipping it hands a
/// front end a page in which nothing is unread: the read it then asks for
/// names messages the daemon was told were already seen, no receipt goes out,
/// and the badge comes back on the next hydration.
///
/// Returns whatever budget the page did not spend, for a caller walking a
/// PN/LID pair a page at a time.
fn mark_unread_tail(messages: &mut [ChatMessage], unread: u32) -> u32 {
    let mut remaining = unread;
    for msg in messages.iter_mut().rev() {
        if remaining == 0 {
            break;
        }
        if !msg.is_from_me {
            msg.is_read = false;
            remaining -= 1;
        }
    }
    remaining
}

fn stored_to_chat_message(stored: oxidezap_chat_store::StoredMessage) -> ChatMessage {
    // The stored proto still carries the media envelope: hydrate thumbnails +
    // download info so historical media renders and stays fetchable, instead
    // of degrading to a [kind] text row until a live redelivery.
    let media = (!stored.revoked)
        .then_some(stored.message.as_deref())
        .flatten()
        .and_then(|m| media_metadata(m.get_base_message()));
    let content = match (&stored.text, stored.revoked) {
        (_, true) => "[Message deleted]".to_string(),
        (Some(text), _) => text.clone(),
        (None, _) if media.is_some() => String::new(),
        (None, _) => format!("[{}]", stored.kind.as_str()),
    };
    // Outgoing ticks come from the stored delivery status; incoming default
    // to read and load_history un-reads the chat's unread tail (per-incoming
    // read state lives on the chat cursor, not the row).
    let is_read = if stored.from_me {
        matches!(
            stored.status,
            oxidezap_chat_store::MessageStatus::Read | oxidezap_chat_store::MessageStatus::Played
        )
    } else {
        true
    };
    let quoted = (!stored.revoked)
        .then_some(stored.message.as_deref())
        .flatten()
        .and_then(|m| quoted_from(m.get_base_message()));
    ChatMessage {
        id: stored.id,
        sender: stored.sender_jid.to_string(),
        sender_name: None,
        content,
        timestamp: stored.timestamp,
        is_from_me: stored.from_me,
        is_read,
        media,
        reactions: std::collections::HashMap::new(),
        // The store has tracked the real delivery state all along; the UI used
        // to flatten it to a bool and lose the delivered/read distinction that
        // the second tick exists to show.
        status: if stored.from_me {
            store_status(stored.status)
        } else {
            MessageStatus::default()
        },
        quoted,
        revoked: stored.revoked,
        system: None,
    }
}

/// Map the store's durable delivery state onto the one the UI draws.
fn store_status(status: oxidezap_chat_store::MessageStatus) -> MessageStatus {
    use oxidezap_chat_store::MessageStatus as Stored;
    match status {
        // Error is terminal for from_me rows (a nack or a local send failure),
        // so hydration restores the failure indicator rather than grey ticks.
        Stored::Error => MessageStatus::Failed,
        Stored::Pending => MessageStatus::Pending,
        Stored::ServerAck => MessageStatus::Sent,
        Stored::Delivered => MessageStatus::Delivered,
        // Played is Read plus "and listened to it"; the ticks are the same.
        Stored::Read | Stored::Played => MessageStatus::Read,
    }
}

/// The reply context for a quote the front end composed.
///
/// The quoted copy is rebuilt from the preview rather than kept: nothing
/// stores the original protobuf, and the preview is what the quote bar shows
/// on both sides. Its id and its author are what actually thread the reply,
/// and those are exact.
fn quote_context(quoted: &oxidezap_core::QuotedMessage) -> wa::ContextInfo {
    use oxidezap_core::QuotedKind;
    use whatsapp_rust::buffa::MessageField;

    let caption = (!quoted.preview.is_empty()).then(|| quoted.preview.clone());
    // The body's *kind*, not a sentence about it. Rebuilding every quote as
    // plain text sent the recipient the word "Photo" where their client would
    // have drawn a photo — and `QuotedKind` exists precisely to carry that
    // distinction across a preview that cannot.
    let original = match quoted.kind {
        Some(QuotedKind::Image) => wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                caption,
                ..Default::default()
            }),
            ..Default::default()
        },
        Some(QuotedKind::Video) => wa::Message {
            video_message: MessageField::some(wa::message::VideoMessage {
                caption,
                ..Default::default()
            }),
            ..Default::default()
        },
        Some(QuotedKind::Audio) => wa::Message {
            audio_message: MessageField::some(wa::message::AudioMessage::default()),
            ..Default::default()
        },
        Some(QuotedKind::Document) => wa::Message {
            document_message: MessageField::some(wa::message::DocumentMessage {
                caption,
                ..Default::default()
            }),
            ..Default::default()
        },
        Some(QuotedKind::Sticker) => wa::Message {
            sticker_message: MessageField::some(wa::message::StickerMessage::default()),
            ..Default::default()
        },
        None => wa::Message {
            conversation: Some(quoted.preview.clone()),
            ..Default::default()
        },
    };
    whatsapp_rust::wacore::proto_helpers::build_quote_context(
        quoted.message_id.clone(),
        quoted.sender.clone(),
        &original,
    )
}

/// Who this device is linked as, off the device store.
///
/// Both fields are optional because both can genuinely be unknown: a device
/// that has paired but never synced its profile has no push name, and the
/// account row says so rather than inventing one.
fn account_event(client: &Arc<Client>) -> UiEvent {
    let device = client.persistence_manager().get_device_snapshot();
    UiEvent::AccountUpdated {
        name: Some(device.push_name.clone()).filter(|name| !name.is_empty()),
        jid: device.pn.as_ref().map(ToString::to_string),
        lid: device.lid.as_ref().map(ToString::to_string),
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

/// What a debounced window of store invalidations forces a reload to cover.
///
/// The store names the chat behind every message-level change, and a change
/// confined to message rows leaves the list's order, membership and names
/// alone — so the window can be answered by rebuilding just those chats.
/// Anything else in the window (or a gap in it) widens the reload back to the
/// whole list, because that is the only load allowed to prune.
#[derive(Debug, PartialEq, Eq)]
enum ReloadScope {
    /// Only these chats' message sets moved.
    Chats(HashSet<String>),
    /// Rebuild the display list.
    Everything,
}

impl ReloadScope {
    fn empty() -> Self {
        ReloadScope::Chats(HashSet::new())
    }

    /// Fold one invalidation in. `None` is a lagged receiver: what it dropped
    /// is unknowable, so it counts as everything.
    fn widen(&mut self, change: Option<&StoreChange>) {
        match (&mut *self, change) {
            (ReloadScope::Everything, _) => {}
            // Contacts too: a push name landing after the chat row must
            // refresh chats stuck on the JID placeholder, and naming is
            // resolved for the whole list at load time.
            (_, None) | (_, Some(StoreChange::Chats | StoreChange::Contacts)) => {
                *self = ReloadScope::Everything;
            }
            (ReloadScope::Chats(chats), Some(StoreChange::Messages { chat })) => {
                chats.insert(chat.to_non_ad_string());
            }
        }
    }

    /// The chats to rebuild, or `None` for the whole list.
    fn chats(&self) -> Option<&HashSet<String>> {
        match self {
            ReloadScope::Chats(chats) => Some(chats),
            ReloadScope::Everything => None,
        }
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        ChatEntry, ChatStore, Client, LoadedHistory, NameBook, ReadBoundary, ReloadScope,
        SqliteStore, StoreChange, WhatsAppClient, apply_status_views, chat_cursor, media_metadata,
        merge_alias_history_messages, message_cursor, parse_chat_cursor, parse_message_cursor,
        read_message_range, still_preview,
    };
    use oxidezap_core::{Chat, ChatMessage, MessageStatus, fallback_chat_name};
    use std::sync::Arc;
    use whatsapp_rust::buffa::MessageField;
    use whatsapp_rust::wacore::proto_helpers::MessageBuilderExt;
    use whatsapp_rust::wacore_binary::Jid;
    use whatsapp_rust::waproto::whatsapp as wa;

    /// A name book with nothing behind its handle: the history paths hand the
    /// store in with every call, and only the live paths read it from there.
    fn book() -> NameBook {
        NameBook::new(Arc::new(super::Mutex::new(None)))
    }

    /// A store-hydrated chat list must label a photo the way the live path
    /// does. The store's preview column holds the newest message's TEXT, and a
    /// photo with no caption has none — the bubble does, and a row rendered
    /// from the empty column reads as "No messages" over a chat that plainly
    /// has one.
    #[tokio::test]
    async fn hydrated_preview_labels_a_captionless_photo() {
        let (chat_store, client) = test_session("preview-photo").await;
        let photo = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                mimetype: Some("image/jpeg".into()),
                jpeg_thumbnail: Some(vec![1, 2, 3]),
                ..Default::default()
            }),
            ..Default::default()
        };
        feed(&chat_store, incoming(photo, "MSG-IMG", 1_700_000_000)).await;

        let LoadedHistory {
            chats, complete, ..
        } = WhatsAppClient::load_history(&chat_store, &client, &book())
            .await
            .expect("history loads");
        assert!(complete);
        assert_eq!(chats[0].last_message.as_deref(), Some("📷 Photo"));
    }

    /// Same gap at the other end: a revoked newest message clears the store's
    /// preview, and the thread shows a tombstone bubble. The list has to agree
    /// with it rather than claim the chat is empty.
    #[tokio::test]
    async fn hydrated_preview_labels_a_revoked_newest_message() {
        let (chat_store, client) = test_session("preview-revoked").await;
        feed(
            &chat_store,
            incoming(wa::Message::text("oops"), "MSG-R", 1_700_000_000),
        )
        .await;
        let revoke = wa::Message {
            protocol_message: MessageField::some(wa::message::ProtocolMessage {
                key: MessageField::some(wa::MessageKey {
                    id: Some("MSG-R".into()),
                    ..Default::default()
                }),
                r#type: Some(wa::message::protocol_message::Type::REVOKE),
                ..Default::default()
            }),
            ..Default::default()
        };
        feed(&chat_store, incoming(revoke, "MSG-R2", 1_700_000_010)).await;

        let LoadedHistory { chats, .. } =
            WhatsAppClient::load_history(&chat_store, &client, &book())
                .await
                .expect("history loads");
        assert_eq!(chats[0].last_message.as_deref(), Some("[Message deleted]"));
    }

    /// The store's own text still wins where it has one.
    #[tokio::test]
    async fn hydrated_preview_prefers_the_stored_text() {
        let (chat_store, client) = test_session("preview-text").await;
        feed(
            &chat_store,
            incoming(wa::Message::text("bom dia"), "MSG-T", 1_700_000_000),
        )
        .await;

        let LoadedHistory { chats, .. } =
            WhatsAppClient::load_history(&chat_store, &client, &book())
                .await
                .expect("history loads");
        assert_eq!(chats[0].last_message.as_deref(), Some("bom dia"));
    }

    /// A page opened on a chat nobody has read comes back saying every row in
    /// it has been read, so the read the front end then asks for names
    /// messages the daemon was told were already seen and no receipt goes out.
    #[tokio::test]
    async fn a_page_of_an_unread_chat_comes_back_unread() {
        let (chat_store, client) = test_session("page-unread").await;
        for (n, id) in ["MSG-P1", "MSG-P2", "MSG-P3"].iter().enumerate() {
            feed(
                &chat_store,
                incoming(wa::Message::text("oi"), id, 1_700_000_000 + n as i64),
            )
            .await;
        }

        let page = WhatsAppClient::message_page(
            &chat_store,
            &client,
            &book(),
            TEST_PEER.to_string(),
            None,
            50,
        )
        .await
        .expect("page loads");

        assert_eq!(page.items.len(), 3);
        assert!(
            page.items.iter().all(|m| !m.is_read),
            "nothing in the chat has been read, so nothing in its page may say it was"
        );
    }

    /// The stream reaches this side ordered and used to be handled on a task
    /// per event, so a call's later stanza could run before the offer that
    /// made it: the removal finds nothing, the offer's task files the call
    /// after it, and a card rings on for a call that is over.
    #[test]
    fn a_calls_later_stanza_is_handled_behind_its_offer() {
        use whatsapp_rust::wacore::types::call::{CallAction, IncomingCall, MissedCall};
        use whatsapp_rust::wacore::types::events::Event;

        let call_id = "CALL-ORDER-1";
        let peer: Jid = TEST_PEER.parse().expect("test JID");
        let at = whatsapp_rust::wacore::time::from_secs(1_700_000_000).expect("test timestamp");
        let offer = Event::IncomingCall(IncomingCall::new_for_test(
            peer.clone(),
            "STANZA-1".to_string(),
            at,
            CallAction::Offer {
                call_id: call_id.to_string(),
                call_creator: peer.clone(),
                caller_pn: None,
                caller_country_code: None,
                device_class: None,
                joinable: true,
                is_video: false,
                audio: Vec::new(),
                group_jid: None,
            },
        ));
        let missed = Event::MissedCall(MissedCall::new(
            peer,
            call_id.to_string(),
            at,
            whatsapp_rust::wacore::types::call::MissedReason::Offline,
        ));

        assert_eq!(
            super::lane_of(
                super::event_subject(&offer)
                    .map(|s| s.as_written())
                    .as_deref()
            ),
            super::lane_of(
                super::event_subject(&missed)
                    .map(|s| s.as_written())
                    .as_deref()
            ),
        );
    }

    /// A batch can span chats — the store's own fixtures build one over a
    /// hundred of them — and a lane keeps one chat's order. Sent whole on the
    /// first message's lane, a receipt for a later chat in the batch runs on
    /// that chat's own lane and can overtake the message it answers.
    #[test]
    fn a_batch_spanning_chats_reaches_every_lane_it_is_about() {
        use whatsapp_rust::wacore::types::events::{
            BatchOrigin, Event, InboundMessage, MessageBatch,
        };
        use whatsapp_rust::wacore::types::message::{MessageInfo, MessageSource};

        let chats = ["1@s.whatsapp.net", "2@s.whatsapp.net", "3@s.whatsapp.net"];
        let messages: Arc<[InboundMessage]> = chats
            .iter()
            .enumerate()
            .map(|(n, chat)| {
                let info = MessageInfo {
                    source: MessageSource {
                        chat: chat.parse().expect("test JID"),
                        sender: chat.parse().expect("test JID"),
                        ..Default::default()
                    },
                    id: format!("MSG-BATCH-{n}"),
                    timestamp: whatsapp_rust::wacore::time::from_secs(1_700_000_000)
                        .expect("test timestamp"),
                    ..Default::default()
                };
                InboundMessage::builder()
                    .message(Arc::new(wa::Message::text("oi")))
                    .info(Arc::new(info))
                    .build()
            })
            .collect();
        let batch = Arc::new(Event::Messages(
            MessageBatch::builder()
                .messages(messages)
                .origin(BatchOrigin::OfflineDrain)
                .build(),
        ));

        let parts = super::split_by_subject(&batch);
        assert_eq!(parts.len(), 3, "one per chat it is about");
        let mut subjects: Vec<String> = parts
            .iter()
            .filter_map(|part| super::event_subject(part).map(|s| s.as_written()))
            .collect();
        subjects.sort();
        assert_eq!(subjects, chats);
        // How the batch was delivered is as true of one chat's share of it as
        // of the whole, and it is what decides whether media is fetched.
        for part in &parts {
            let Event::Messages(batch) = &**part else {
                panic!("a batch splits into batches");
            };
            assert!(matches!(batch.origin, BatchOrigin::OfflineDrain));
            assert_eq!(batch.iter().count(), 1);
        }

        // And a batch about one chat is not rebuilt at all.
        let single = incoming(wa::Message::text("oi"), "MSG-ONE", 1_700_000_000);
        assert_eq!(super::split_by_subject(&Arc::new(single)).len(), 1);
    }

    /// Two chats are not each other's business, so they do not queue behind
    /// one another; an event about the account is about neither.
    #[test]
    fn events_are_keyed_by_what_they_are_about() {
        assert_eq!(
            super::event_subject(&incoming(
                wa::Message::text("oi"),
                "MSG-LANE",
                1_700_000_000
            ))
            .map(|s| s.as_written())
            .as_deref(),
            Some(TEST_PEER)
        );
        assert_eq!(
            super::event_subject(&incoming_in(
                "120363000000000001@g.us",
                wa::Message::text("oi"),
                "MSG-LANE-2",
                1_700_000_000,
            ))
            .map(|s| s.as_written())
            .as_deref(),
            Some("120363000000000001@g.us")
        );

        // Presence is about a person and a group update about a group, and
        // both handlers go to the store. On the session-wide lane a burst of
        // either queued in front of `Connected`, `PairingQrCode` and
        // `LoggedOut` -- the events a window waits on to draw anything.
        let presence = whatsapp_rust::wacore::types::events::Event::Presence(
            whatsapp_rust::wacore::types::events::PresenceUpdate::builder()
                .from(TEST_PEER.parse().unwrap())
                .unavailable(false)
                .build(),
        );
        assert_eq!(
            super::event_subject(&presence)
                .map(|s| s.as_written())
                .as_deref(),
            Some(TEST_PEER)
        );
        assert_ne!(
            super::lane_of(
                super::event_subject(&presence)
                    .map(|s| s.as_written())
                    .as_deref()
            ),
            super::lane_of(None),
            "and so is not on the account's own lane"
        );
    }

    /// Reconnecting after a while offline hands over a batch of hundreds, and
    /// fetching a picture per message before the first bubble reaches the
    /// window spends the whole reconnection on it. The same question decides
    /// a picture nobody is going to look at soon enough to be worth the
    /// bytes.
    #[test]
    fn a_backlog_is_not_a_reason_to_fetch_every_picture() {
        // Live and small: the one case worth the round trip.
        assert!(WhatsAppClient::worth_fetching_now(true, Some(64 * 1024)));
        assert!(WhatsAppClient::worth_fetching_now(true, None));

        assert!(!WhatsAppClient::worth_fetching_now(false, Some(64 * 1024)));
        assert!(!WhatsAppClient::worth_fetching_now(false, None));
        assert!(
            !WhatsAppClient::worth_fetching_now(true, Some(WhatsAppClient::EAGER_MEDIA_BYTES + 1)),
            "past the ceiling the thumbnail shows and the bytes stay retryable"
        );
    }

    /// The quote bar above a reply named an unknown contact while the bubbles
    /// from the same person, an inch above it, carried their name: the
    /// participant went onto the row exactly as the envelope spelled it,
    /// device suffix and all, and `Chat::quoted_author` looks a participant
    /// up by exact string.
    #[tokio::test]
    async fn a_quoted_author_is_filed_where_their_bubbles_are() {
        use whatsapp_rust::waproto::buffa;
        use whatsapp_rust::waproto::whatsapp::message;

        let (chat_store, client) = test_session("quoted-author").await;
        let reply = wa::Message {
            extended_text_message: buffa::MessageField::some(message::ExtendedTextMessage {
                text: Some("e o áudio?".to_string()),
                context_info: buffa::MessageField::some(wa::ContextInfo {
                    stanza_id: Some("ORIGINAL".to_string()),
                    // As a sending device spells itself.
                    participant: Some(TEST_PEER.replace('@', ":12@")),
                    quoted_message: buffa::MessageField::some(wa::Message {
                        conversation: Some("ping".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        feed(&chat_store, incoming(reply, "MSG-QUOTE", 1_700_000_000)).await;

        let page = WhatsAppClient::message_page(
            &chat_store,
            &client,
            &book(),
            TEST_PEER.to_string(),
            None,
            50,
        )
        .await
        .expect("page loads");
        let quoted = page.items[0].quoted.as_ref().expect("this is a reply");
        assert_eq!(quoted.sender, TEST_PEER);
    }

    const TEST_PEER: &str = "559900000001@s.whatsapp.net";

    /// A chat store and a client over one in-memory database, with no network:
    /// `Bot::build` only opens the store, and `load_history` needs the client
    /// solely for the PN/LID mapping lookups that resolve chat identity.
    async fn test_session(name: &str) -> (Arc<ChatStore>, Arc<Client>) {
        let store = SqliteStore::new(&format!(
            "file:oxidezap-session-{name}?mode=memory&cache=shared"
        ))
        .await
        .expect("in-memory store");
        let chat_store = ChatStore::new(&store).await.expect("chat store");
        let bot = whatsapp_rust::bot::Bot::builder()
            .with_backend(store)
            .build()
            .await
            .expect("offline bot");
        (chat_store, bot.client())
    }

    fn incoming(
        message: wa::Message,
        id: &str,
        ts_secs: i64,
    ) -> whatsapp_rust::wacore::types::events::Event {
        incoming_in(TEST_PEER, message, id, ts_secs)
    }

    fn incoming_in(
        chat: &str,
        message: wa::Message,
        id: &str,
        ts_secs: i64,
    ) -> whatsapp_rust::wacore::types::events::Event {
        use whatsapp_rust::wacore::types::events::{
            BatchOrigin, Event, InboundMessage, MessageBatch,
        };
        use whatsapp_rust::wacore::types::message::{MessageInfo, MessageSource};

        let info = MessageInfo {
            source: MessageSource {
                chat: chat.parse().expect("test JID"),
                sender: chat.parse().expect("test JID"),
                ..Default::default()
            },
            id: id.to_string(),
            timestamp: whatsapp_rust::wacore::time::from_secs(ts_secs).expect("test timestamp"),
            ..Default::default()
        };
        Event::Messages(
            MessageBatch::builder()
                .messages(Arc::from([InboundMessage::builder()
                    .message(Arc::new(message))
                    .info(Arc::new(info))
                    .build()]))
                .origin(BatchOrigin::Live)
                .build(),
        )
    }

    async fn feed(chat_store: &Arc<ChatStore>, event: whatsapp_rust::wacore::types::events::Event) {
        chat_store.handler().handle_event(Arc::new(event));
        chat_store.flush().await.expect("flush");
    }

    fn messages_change(chat: &str) -> StoreChange {
        StoreChange::Messages {
            chat: chat.parse().expect("test JID"),
        }
    }

    /// Acks and receipts are message-level, and a peer answers a single send
    /// with several of them; rebuilding the whole chat list for each is the
    /// work this narrowing exists to skip.
    #[test]
    fn message_only_invalidations_narrow_the_reload() {
        let mut scope = ReloadScope::empty();
        scope.widen(Some(&messages_change("12025550143@s.whatsapp.net")));
        // The same chat named twice in one window is one chat to rebuild.
        scope.widen(Some(&messages_change("12025550143:12@s.whatsapp.net")));
        scope.widen(Some(&messages_change("120363000000000001@g.us")));

        let chats = scope.chats().expect("narrowed reload");
        assert_eq!(chats.len(), 2);
        assert!(chats.contains("12025550143@s.whatsapp.net"));
        assert!(chats.contains("120363000000000001@g.us"));
    }

    /// Ordering, membership and naming are resolved across the whole list, and
    /// only a whole-list load may prune it.
    #[test]
    fn list_level_invalidations_widen_the_reload() {
        for change in [StoreChange::Chats, StoreChange::Contacts] {
            let mut scope = ReloadScope::empty();
            scope.widen(Some(&messages_change("12025550143@s.whatsapp.net")));
            scope.widen(Some(&change));
            scope.widen(Some(&messages_change("120363000000000001@g.us")));
            assert_eq!(scope.chats(), None, "{change:?} must force a full reload");
        }
    }

    /// A lagged receiver dropped changes it cannot name, so the reload has to
    /// assume the worst of them.
    #[test]
    fn a_gap_in_the_window_widens_the_reload() {
        let mut scope = ReloadScope::empty();
        scope.widen(Some(&messages_change("12025550143@s.whatsapp.net")));
        scope.widen(None);
        assert_eq!(scope.chats(), None);
    }

    #[test]
    fn history_fallbacks_do_not_expose_internal_lids() {
        let lid: Jid = "111222333444555@lid".parse().expect("test LID");
        let pn: Jid = "12025550143@s.whatsapp.net".parse().expect("test PN");
        let group: Jid = "120363000000000001@g.us".parse().expect("test group");

        assert_eq!(fallback_chat_name(&lid), "Unknown contact");
        assert_eq!(fallback_chat_name(&pn), "+12025550143");
        assert_eq!(fallback_chat_name(&group), "Unnamed group");
    }

    #[test]
    fn alias_history_unread_deduplicates_only_matching_messages() {
        let message = |id: &str| ChatMessage {
            id: id.to_string(),
            sender: "12025550143@s.whatsapp.net".to_string(),
            sender_name: None,
            content: id.to_string(),
            timestamp: whatsapp_rust::wacore::time::now_utc(),
            is_from_me: false,
            is_read: false,
            media: None,
            reactions: HashMap::new(),
            status: MessageStatus::default(),
            quoted: None,
            revoked: false,
            system: None,
        };
        let mut chat = Chat::new("111222333444555@lid".to_string());
        chat.messages = vec![message("MSG-A"), message("MSG-B")];
        chat.unread_count = 2;

        merge_alias_history_messages(
            &mut chat,
            vec![message("MSG-B"), message("MSG-C"), message("MSG-D")],
            3,
        );
        assert_eq!(chat.unread_count, 4);
        assert_eq!(chat.messages.len(), 4);

        merge_alias_history_messages(
            &mut chat,
            vec![message("MSG-B"), message("MSG-C"), message("MSG-D")],
            3,
        );
        assert_eq!(chat.unread_count, 4);
    }

    /// A status row hydrates read or unread from the broadcast's unread
    /// cursor, which counts everybody's updates at once and cannot say which
    /// of them was looked at. Without the views put back, every restart
    /// offered watched updates as new.
    #[test]
    fn watched_updates_come_back_watched() {
        let update = |id: &str, from_me: bool| ChatMessage {
            id: id.to_string(),
            sender: "12025550143@s.whatsapp.net".to_string(),
            sender_name: None,
            content: id.to_string(),
            timestamp: whatsapp_rust::wacore::time::now_utc(),
            is_from_me: from_me,
            is_read: false,
            media: None,
            reactions: HashMap::new(),
            status: MessageStatus::default(),
            quoted: None,
            revoked: false,
            system: None,
        };
        let mut broadcast = Chat::new(oxidezap_core::STATUS_BROADCAST_JID.to_string());
        broadcast.messages = vec![
            update("SEEN", false),
            update("NEW", false),
            update("MINE", true),
        ];
        let mut conversation = Chat::new("12025550143@s.whatsapp.net".to_string());
        conversation.messages = vec![update("SEEN", false)];

        let watched = ["SEEN".to_string(), "MINE".to_string()]
            .into_iter()
            .collect();
        let mut chats = vec![broadcast, conversation];
        apply_status_views(&mut chats, &watched);

        assert!(
            chats[0].messages[0].is_read,
            "watched update comes back watched"
        );
        assert!(!chats[0].messages[1].is_read, "one nobody opened stays new");
        assert!(
            !chats[0].messages[2].is_read,
            "our own row carries the peer's read ticks; a local view must not set them"
        );
        assert!(
            !chats[1].messages[0].is_read,
            "a conversation is not the broadcast, whatever ids collide"
        );
    }

    #[test]
    fn a_still_with_nothing_to_download_is_not_offered_as_a_preview() {
        let orphan = still_preview(vec![1, 2, 3], "image/png", "image/webp".into(), false);
        assert!(!orphan.is_preview);
        assert_eq!(orphan.mime, "image/png");

        let fetchable = still_preview(vec![1, 2, 3], "image/png", "image/webp".into(), true);
        assert!(fetchable.is_preview);

        // No bytes in hand: the mime describes the file being waited for.
        let empty = still_preview(Vec::new(), "image/png", "image/webp".into(), true);
        assert!(!empty.is_preview);
        assert_eq!(empty.mime, "image/webp");
    }

    #[test]
    fn historical_sticker_keeps_thumbnail_without_download_metadata() {
        let message = wa::Message {
            sticker_message: MessageField::some(wa::message::StickerMessage {
                png_thumbnail: Some(vec![1, 2, 3]),
                width: Some(64),
                height: Some(64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let media = media_metadata(&message).expect("sticker metadata");
        assert_eq!(media.data.as_slice(), [1, 2, 3]);
        assert_eq!(media.mime_type, "image/png");
        assert!(media.downloadable.is_none());
        assert!(!media.data_is_preview);
    }

    #[test]
    fn participant_keyed_read_ranges_include_incoming_participants() {
        let group: Jid = "120363000000000001@g.us".parse().expect("test group");
        let boundary: ReadBoundary = (
            1_700_000_000,
            vec![
                (
                    "incoming".to_string(),
                    false,
                    Some("12025550143@s.whatsapp.net".to_string()),
                ),
                ("outgoing".to_string(), true, None),
            ],
        );

        let range = read_message_range(&group, boundary);
        let incoming = range.messages[0].key.as_option().expect("incoming key");
        let outgoing = range.messages[1].key.as_option().expect("outgoing key");

        assert_eq!(
            incoming.participant.as_deref(),
            Some("12025550143@s.whatsapp.net")
        );
        assert_eq!(outgoing.participant, None);

        let status: Jid = "status@broadcast".parse().expect("test status");
        let boundary = (
            1_700_000_000,
            vec![(
                "status".to_string(),
                false,
                Some("12025550144@s.whatsapp.net".to_string()),
            )],
        );
        let range = read_message_range(&status, boundary);
        assert_eq!(
            range.messages[0]
                .key
                .as_option()
                .expect("status key")
                .participant
                .as_deref(),
            Some("12025550144@s.whatsapp.net")
        );
    }

    /// A narrowed load is about the chats somebody named, and the page it
    /// starts from is the hundred most recently active ones. A chat past that
    /// window was filtered down to nothing — and a load with nothing in it
    /// publishes nothing, so the invalidation that asked for it was spent in
    /// silence and every front end stayed on rows that had changed.
    #[tokio::test]
    async fn a_scoped_load_finds_a_chat_the_page_left_out() {
        use whatsapp_rust::wacore::types::events::{
            BatchOrigin, Event, InboundMessage, MessageBatch,
        };
        use whatsapp_rust::wacore::types::message::{MessageInfo, MessageSource};

        let (chat_store, client) = test_session("scoped-load-beyond-the-page").await;
        // The target is the oldest, so the hundred newer ones fill the page
        // ahead of it. One batch, because a hundred flushes is a hundred
        // transactions for a fact this test states once.
        let target = "559900000000@s.whatsapp.net";
        let mut batch = Vec::new();
        for (index, jid) in std::iter::once(target.to_string())
            .chain((1..=100).map(|n| format!("55990000{n:04}@s.whatsapp.net")))
            .enumerate()
        {
            let sender: Jid = jid.parse().expect("test JID");
            batch.push(
                InboundMessage::builder()
                    .message(Arc::new(wa::Message::text("oi")))
                    .info(Arc::new(MessageInfo {
                        source: MessageSource {
                            chat: sender.clone(),
                            sender,
                            ..Default::default()
                        },
                        id: format!("MSG-{index}"),
                        // Ascending, so the target's is the oldest.
                        timestamp: whatsapp_rust::wacore::time::from_secs(
                            1_700_000_000 + index as i64,
                        )
                        .expect("test timestamp"),
                        ..Default::default()
                    }))
                    .build(),
            );
        }
        feed(
            &chat_store,
            Event::Messages(
                MessageBatch::builder()
                    .messages(Arc::from(batch))
                    .origin(BatchOrigin::Live)
                    .build(),
            ),
        )
        .await;

        let only = std::collections::HashSet::from([target.to_string()]);
        let LoadedHistory {
            chats,
            complete,
            next,
        } = WhatsAppClient::load_history_scoped(&chat_store, &client, Some(&only), &book())
            .await
            .expect("history loads");
        assert!(
            next.is_none(),
            "a narrowed load is not a position in the list"
        );

        assert!(!complete, "a narrowed load is never the whole list");
        assert_eq!(
            chats
                .iter()
                .map(|chat| chat.jid.as_str())
                .collect::<Vec<_>>(),
            vec![target],
            "the chat it was asked about, page or no page"
        );
        assert_eq!(chats[0].messages.len(), 1);
    }

    /// A cursor is this crate's to write and to read, and the only thing that
    /// makes that safe is that the two agree.
    #[test]
    fn a_message_cursor_survives_the_round_trip() {
        let mut row = oxidezap_chat_store::StoredMessage {
            chat_jid: "5599000000001@s.whatsapp.net".parse().unwrap(),
            id: "3EB0".to_string(),
            sender_jid: "5599000000001@s.whatsapp.net".parse().unwrap(),
            from_me: false,
            timestamp: whatsapp_rust::wacore::time::from_millis(1_700_000_000_123).unwrap(),
            kind: oxidezap_chat_store::MessageKind::Text,
            text: Some("olá".to_string()),
            message: None,
            status: oxidezap_chat_store::MessageStatus::Delivered,
            revoked: false,
            edited_at: None,
            starred: false,
            seq: 4242,
        };
        let token = message_cursor(&row);
        assert_eq!(token, "m1:1700000000123:4242");
        let cursor = parse_message_cursor(&token).expect("reads back");
        assert_eq!(cursor.timestamp_ms, 1_700_000_000_123);
        assert_eq!(cursor.seq, 4242);

        // A page boundary inside a same-second run is exactly what the seq is
        // for: two rows with one timestamp are two positions.
        row.seq = 4243;
        assert_ne!(message_cursor(&row), token);

        assert!(
            parse_message_cursor("c1:-:1:a@s.whatsapp.net").is_none(),
            "not this list's"
        );
        assert!(parse_message_cursor("m1:notanumber:1").is_none());
        assert!(parse_message_cursor("").is_none());
    }

    /// The address goes last and is not split on: a device JID carries a
    /// colon of its own, and a cursor that lost the tail of one would page
    /// A load that stopped at its limit knows where it stopped, and saying so
    /// is what stops a window's first "load more" from being the page it was
    /// just handed: the attach load left no cursor, so the only way to obtain
    /// one was to ask for those hundred rows all over again.
    #[tokio::test]
    async fn a_truncated_load_says_where_the_list_continues() {
        let (chat_store, client) = test_session("history-cursor").await;
        let rows = WhatsAppClient::HISTORY_CHAT_LIMIT + 5;
        for n in 0..rows {
            let chat = format!("5599{n:09}@s.whatsapp.net");
            chat_store.handler().handle_event(Arc::new(incoming_in(
                &chat,
                wa::Message::text("olá"),
                &format!("MSG-{n:04}"),
                1_700_000_000 + n,
            )));
        }
        chat_store.flush().await.expect("flush");

        let LoadedHistory {
            chats,
            complete,
            next,
        } = WhatsAppClient::load_history(&chat_store, &client, &book())
            .await
            .expect("history loads");
        assert!(!complete, "more chats than the load carries");
        let token = next.expect("a load that stopped at its limit stopped somewhere");

        // What continues from there is rows this load did not carry, which is
        // the whole point: the page after a load is not the load.
        let after = parse_chat_cursor(&token).expect("this crate reads its own token");
        let page = chat_store
            .chats_page(false, Some(after), 5)
            .await
            .expect("a page after the load");
        assert!(!page.is_empty(), "there is more behind a truncated load");
        let carried: std::collections::HashSet<String> =
            chats.iter().map(|chat| chat.jid.clone()).collect();
        for entry in &page {
            assert!(
                !carried.contains(&entry.jid.to_string()),
                "{} was in the load and in the page after it",
                entry.jid
            );
        }
    }

    /// from a chat that does not exist.
    #[test]
    fn a_chat_cursor_keeps_an_address_with_a_colon_in_it() {
        let entry = ChatEntry {
            jid: "5599000000001:57@s.whatsapp.net".parse().unwrap(),
            name: None,
            last_message_at: Some(
                whatsapp_rust::wacore::time::from_millis(1_700_000_000_123).unwrap(),
            ),
            last_message_preview: None,
            last_message_kind: None,
            unread_count: 0,
            pinned_at: Some(whatsapp_rust::wacore::time::from_millis(1_699_999_999_000).unwrap()),
            muted_until: None,
            archived: false,
            ephemeral_expiration: None,
        };
        let token = chat_cursor(&entry);
        assert_eq!(
            token,
            "c1:1699999999000:1700000000123:5599000000001:57@s.whatsapp.net"
        );
        let cursor = parse_chat_cursor(&token).expect("reads back");
        assert_eq!(cursor.pinned_at_ms, Some(1_699_999_999_000));
        assert_eq!(cursor.last_message_ts, 1_700_000_000_123);
        assert_eq!(cursor.jid, "5599000000001:57@s.whatsapp.net");

        // The unpinned run, which is where most of the list lives.
        let plain = parse_chat_cursor("c1:-:1700000000123:5599000000001@s.whatsapp.net")
            .expect("reads back");
        assert_eq!(plain.pinned_at_ms, None);
        assert_eq!(plain.jid, "5599000000001@s.whatsapp.net");
    }

    #[test]
    fn an_unreadable_pin_does_not_read_back_as_unpinned() {
        assert!(parse_chat_cursor("c1:xx:1700000000123:5599000000001@s.whatsapp.net").is_none());
        assert!(parse_chat_cursor("c1::1700000000123:5599000000001@s.whatsapp.net").is_none());
    }
}
