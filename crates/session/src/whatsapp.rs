//! WhatsApp client wrapper for UI integration

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use log::{debug, error, info, warn};
use oxidezap_chat_store::{ChatEntry, ChatStore, StoreChange};
use tokio::sync::{Mutex, mpsc};
use whatsapp_rust::bot::Bot;
use whatsapp_rust::client::Client;
use whatsapp_rust::store::SqliteStore;
use whatsapp_rust::voip::{CallHandle, CallTermination};
use whatsapp_rust::wacore::proto_helpers::MessageExt;
use whatsapp_rust::wacore::types::call::{CallAction, IncomingCall as WaIncomingCall};
use whatsapp_rust::wacore::types::events::Event;
use whatsapp_rust::wacore::types::presence::{
    ChatPresence as WaChatPresence, ChatPresenceMedia, ReceiptType,
};
use whatsapp_rust::wacore_binary::jid::{Jid, JidExt, observe_str};
use whatsapp_rust::waproto::whatsapp as wa;

use oxidezap_audio::{spawn_mic, spawn_speaker};
use oxidezap_core::{
    Availability, Chat, ChatMessage, ComposingKind, DownloadableMedia, IncomingCall, MediaContent,
    MediaType, MessageStatus, SystemNotice, UiEvent, fallback_chat_name,
};

use crate::names::NameBook;
use crate::quoting::quoted_from;
use whatsapp_rust::wacore::download::MediaType as DownloadMediaType;

/// Resolve a stable per-user path for the SQLite database. A CWD-relative
/// path would silently split state between launch methods (desktop launcher
/// vs terminal), so prefer the platform data dir and only fall back to the
/// working directory when no home is known.
pub fn resolve_database_path() -> String {
    resolve_database_dir()
        .map(|dir| dir.join(DB_FILE).to_string_lossy().into_owned())
        .unwrap_or_else(|| DB_FILE.to_string())
}

const DB_FILE: &str = "whatsapp.db";

/// Per-user data directory, under the platform data root.
///
/// No fallback to the old `whatsapp-rust-desktop` name: this app has never
/// shipped a release, so there is no installed base to migrate and carrying
/// lookup code for one would be permanent dead weight.
const DATA_DIR: &str = "oxidezap";

/// Delete the local session: device identity, Signal state and chat history all
/// live in the one SQLite file.
///
/// Called after the server ends the session, where reconnecting is pointless —
/// the credentials are dead and pairing mints a new device. A partial wipe is
/// not an option: chat rows are keyed by device id, so keeping them would
/// orphan every one of them behind the new device anyway.
pub fn wipe_local_state() -> std::io::Result<()> {
    let Some(dir) = resolve_database_dir() else {
        return Ok(());
    };
    // -wal and -shm hold committed pages SQLite would replay into a fresh file.
    for suffix in ["", "-wal", "-shm"] {
        let path = dir.join(format!("{DB_FILE}{suffix}"));
        match std::fs::remove_file(&path) {
            Ok(()) => info!("Removed {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn resolve_database_dir() -> Option<std::path::PathBuf> {
    let not_empty = |v: std::ffi::OsString| (!v.is_empty()).then_some(std::path::PathBuf::from(v));
    let data_root = if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .and_then(not_empty)
            .map(|home| home.join("Library/Application Support"))
    } else if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA")
            .and_then(not_empty)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .and_then(not_empty)
                    .map(|profile| profile.join("AppData").join("Local"))
            })
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .and_then(not_empty)
            .or_else(|| {
                std::env::var_os("HOME")
                    .and_then(not_empty)
                    .map(|home| home.join(".local/share"))
            })
    };

    let dir = data_root.map(|root| root.join(DATA_DIR))?;
    // SQLite won't create missing parent directories itself.
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!("Failed to create data dir: {e}; using CWD-relative {DB_FILE}");
        return None;
    }
    Some(dir)
}

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

pub type ReadBoundary = (i64, Vec<(String, bool, Option<String>)>);

type CallAudio = (
    async_channel::Receiver<Vec<i16>>,
    async_channel::Sender<Vec<i16>>,
);

async fn open_call_audio() -> Result<CallAudio, String> {
    tokio::task::spawn_blocking(|| {
        let mic = spawn_mic().map_err(|e| e.to_string())?;
        let speaker = spawn_speaker().map_err(|e| e.to_string())?;
        Ok((mic, speaker))
    })
    .await
    .map_err(|e| format!("audio setup task failed: {e}"))?
}

fn participant_keyed_chat(jid: &Jid) -> bool {
    jid.is_group() || jid.is_broadcast_list() || jid.is_status_broadcast()
}

/// Live call state shared between the event pump and the UI action methods.
#[derive(Clone, Default)]
pub struct CallRegistry {
    /// Ringing offers by call id, consumed by accept/decline.
    pending: Arc<Mutex<HashMap<String, Arc<WaIncomingCall>>>>,
    /// Media-live calls by call id.
    active: Arc<Mutex<HashMap<String, Arc<CallHandle>>>>,
    /// Ids cancelled before any handle existed (the UI's placeholder id while
    /// start_call is still connecting); start_call hangs these up on arrival.
    cancelled: Arc<Mutex<std::collections::HashSet<String>>>,
    /// One mute lane per live call. Pruned against `active` where it grows,
    /// so a call that ends takes its lane with it without every teardown
    /// path having to remember.
    mute: Arc<std::sync::Mutex<HashMap<String, Arc<MuteLane>>>>,
}

/// What keeps a call's mute requests in the order the daemon took them.
///
/// Spawning is not sequencing: two requests spawned in order can start in
/// either one, and the last to reach the wire wins. That is how a rapid
/// unmute-then-mute could leave the microphone open under a state — and every
/// window — showing it muted, with both tasks finding the device in the state
/// they themselves had asked for and so correcting nothing.
#[derive(Default)]
struct MuteLane {
    /// The newest request, stamped on the caller's thread *before* its task
    /// exists. That is the only place the order still exists.
    ///
    /// A `std` lock on purpose: it is taken from a synchronous method and
    /// never held across an await.
    intent: std::sync::Mutex<MuteIntent>,
    /// One announcement in flight per call. The library serializes its own
    /// transitions, but it serializes them in arrival order, which is the
    /// order this exists to stop trusting.
    lane: Mutex<()>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct MuteIntent {
    /// Bumped per request, so a task can ask whether it is still the newest.
    seq: u64,
    muted: bool,
}

/// WhatsApp client wrapper that manages the connection and provides
/// a clean interface for UI operations.
pub struct WhatsAppClient {
    /// Tokio runtime for async operations
    runtime: Arc<tokio::runtime::Runtime>,
    /// Shared client reference
    client_handle: ClientHandle,
    /// Shared UI event sender for sending events from operations like start_call
    ui_sender: UiEventSender,
    /// Live/ringing calls
    calls: CallRegistry,
    /// Durable chat history (same SQLite file as the device store)
    chat_store: ChatStoreHandle,
    /// Tears down `run_client` on retry: without it the replaced client's
    /// thread would keep its runtime and SQLite pool alive forever (bot.run()
    /// reconnects internally and never returns on its own).
    shutdown: Arc<tokio::sync::Notify>,
    /// Asks the history reloader for a full pass.
    ///
    /// The reloader is otherwise driven by store invalidations, which is right
    /// while a front end is attached and wrong the moment one attaches: it has
    /// no chats and nothing has changed, so nothing would arrive until the
    /// next message did.
    reload: Arc<tokio::sync::Notify>,
    /// The session thread, kept joinable.
    ///
    /// `shutdown()` only asks it to stop; the thread still has to disconnect
    /// and close SQLite. Without a handle to wait on, a process that exits
    /// right after asking can die mid-teardown, because Rust does not wait for
    /// threads when `main` returns.
    worker: Option<std::thread::JoinHandle<()>>,
    /// Whether the client has been started
    started: bool,
}

impl WhatsAppClient {
    /// Create a new WhatsApp client wrapper. Errors when the tokio runtime
    /// can't be built (resource exhaustion) so a retry can route to the
    /// error screen instead of panicking the UI thread.
    pub fn new() -> std::io::Result<Self> {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?,
        );

        Ok(Self {
            runtime,
            client_handle: Arc::new(Mutex::new(None)),
            ui_sender: Arc::new(Mutex::new(None)),
            calls: CallRegistry::default(),
            chat_store: Arc::new(Mutex::new(None)),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            reload: Arc::new(tokio::sync::Notify::new()),
            worker: None,
            started: false,
        })
    }

    /// Stop the background run loop so its thread exits and the runtime and
    /// SQLite handles drop. Idempotent; a signal fired before the loop is up
    /// still lands (notify_one stores a permit).
    pub fn shutdown(&self) {
        self.shutdown.notify_one();
    }

    /// Ask the session to stop and wait for it to finish closing.
    ///
    /// The wait is what separates this from [`shutdown`](Self::shutdown): the
    /// thread still has to disconnect the socket and close SQLite, and a
    /// caller that exits without waiting can cut that short. Bounded, so a
    /// wedged session delays exit rather than preventing it.
    ///
    /// Returns whether the thread finished within `timeout`.
    pub fn shutdown_and_join(&mut self, timeout: std::time::Duration) -> bool {
        self.shutdown();
        let Some(handle) = self.worker.take() else {
            return true;
        };

        // `JoinHandle` has no timed join, so wait on a channel the joining
        // thread signals instead: the session still gets to finish, and a
        // stuck one cannot hold the process open forever.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = handle.join();
            let _ = tx.send(());
        });
        rx.recv_timeout(timeout).is_ok()
    }

    /// Get the runtime handle for UI async operations
    #[allow(dead_code)]
    pub fn runtime(&self) -> Arc<tokio::runtime::Runtime> {
        self.runtime.clone()
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
        let runtime = self.runtime.clone();
        let shutdown = self.shutdown.clone();
        let reload = self.reload.clone();

        let spawned = std::thread::Builder::new()
            .name("oxidezap-session".to_string())
            .spawn(move || {
                runtime.block_on(async move {
                    {
                        let mut guard = ui_sender.lock().await;
                        *guard = Some(ui_tx.clone());
                    }
                    Self::run_client(
                        ui_tx,
                        client_handle,
                        calls,
                        chat_store,
                        ui_sender.clone(),
                        shutdown,
                        reload,
                    )
                    .await;
                });
            });
        match spawned {
            Ok(handle) => self.worker = Some(handle),
            Err(_) => {
                self.started = false;
                return Err("failed to spawn WhatsApp client thread");
            }
        }

        Ok(ui_rx)
    }

    /// Internal async function to run the client
    async fn run_client(
        ui_tx: mpsc::UnboundedSender<UiEvent>,
        client_handle: ClientHandle,
        calls: CallRegistry,
        chat_store_handle: ChatStoreHandle,
        ui_sender: UiEventSender,
        shutdown: Arc<tokio::sync::Notify>,
        reload: Arc<tokio::sync::Notify>,
    ) {
        // Device store + durable chat history share one SQLite file (one pool,
        // one WAL writer).
        let db_path = match tokio::task::spawn_blocking(resolve_database_path).await {
            Ok(path) => path,
            Err(e) => {
                error!("Failed to resolve database path: {e}");
                let _ = ui_tx.send(UiEvent::Error("Database initialization failed".to_string()));
                return;
            }
        };
        info!("Opening data database");
        let backend = match SqliteStore::new(&db_path).await {
            Ok(store) => store,
            Err(e) => {
                error!("Failed to create SQLite backend: {}", e);
                let _ = ui_tx.send(UiEvent::Error(format!("Database error: {}", e)));
                return;
            }
        };
        let chat_store = match ChatStore::new(&backend).await {
            Ok(store) => store,
            Err(e) => {
                error!("Failed to open chat store: {}", e);
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

        let ui_tx_clone = ui_tx.clone();
        let calls_clone = calls.clone();
        let ui_sender_clone = ui_sender.clone();
        let names_clone = names.clone();

        // Transport, HTTP client and runtime come from the default cargo
        // features (Tokio WebSocket, ureq, Tokio).
        let bot = match Bot::builder()
            .with_backend(backend)
            .on_event(move |event, client| {
                let ui_tx = ui_tx_clone.clone();
                let calls = calls_clone.clone();
                let ui_sender = ui_sender_clone.clone();
                let names = names_clone.clone();
                async move {
                    Self::handle_event(event, client, ui_tx, calls, ui_sender, names).await;
                }
            })
            .build()
            .await
        {
            Ok(bot) => bot,
            Err(e) => {
                error!("Failed to build bot: {}", e);
                let _ = ui_tx.send(UiEvent::Error(format!("Connection failed: {}", e)));
                return;
            }
        };

        // Hydrate the UI from durable history before the network is even up
        // (bot.run() is what connects). The client is needed here so hydrated
        // JIDs normalize through the same PN->LID mapping live events use.
        match Self::load_history(&chat_store, &bot.client(), &names).await {
            Ok((chats, complete)) if !chats.is_empty() => {
                // The one hydration worth an info line: the reloads that
                // follow are routine and say so at debug.
                info!("Hydrated {} chats from durable history", chats.len());
                let _ = ui_tx.send(UiEvent::HistoryLoaded { chats, complete });
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
            _ = bot.run() => {}
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
                    calls
                        .pending
                        .lock()
                        .await
                        .insert(call_id.clone(), offer.clone());
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
                }
                CallAction::Reject { call_id, .. } => {
                    info!("Call {} rejected by peer", call_id);
                    calls.pending.lock().await.remove(call_id);
                    let _ = ui_tx.send(UiEvent::CallEnded(call_id.clone()));
                }
                CallAction::Terminate { call_id, .. } => {
                    info!("Call {} terminated by peer", call_id);
                    calls.pending.lock().await.remove(call_id);
                    if let Some(handle) = calls.active.lock().await.remove(call_id) {
                        // `hangup_local`, not `terminate`: the peer is the
                        // side that ended this, and answering their
                        // `<terminate>` with one of our own says nothing they
                        // do not already know. Only the local media task and
                        // the registry entry are left to drop.
                        tokio::spawn(async move { handle.hangup_local().await });
                    }
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
                calls.pending.lock().await.remove(&missed.call_id);
                let _ = ui_tx.send(UiEvent::CallEnded(missed.call_id.clone()));
            }
            Event::CallEndedElsewhere(ended) => {
                info!("Call {} handled on another device", ended.call_id);
                calls.pending.lock().await.remove(&ended.call_id);
                let _ = ui_tx.send(UiEvent::CallEndedElsewhere(ended.call_id.clone()));
            }
            Event::Messages(batch) => {
                for inbound in batch.iter() {
                    Self::handle_inbound_message(
                        &inbound.message,
                        &inbound.info,
                        &client,
                        &ui_tx,
                        &names,
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
                    if let Some(name) = names.known(&client, &jid, None).await {
                        named.insert(key, name);
                    }
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

        // Try to extract media content
        let media_result = Self::try_extract_media(base_msg, client).await;

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

    /// Try to extract and download media from a message
    async fn try_extract_media(msg: &wa::Message, _client: &Arc<Client>) -> Option<MediaContent> {
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
            let (data, mime_type, is_animated, data_is_preview) =
                match Self::download_media(_client, sticker, "sticker").await {
                    Some(data) => (data, mime, sticker.is_animated.unwrap_or(false), false),
                    None => (
                        sticker
                            .png_thumbnail
                            .as_ref()
                            .filter(|t| !t.is_empty())
                            .cloned()
                            .unwrap_or_default(),
                        "image/png".to_string(),
                        false,
                        true,
                    ),
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
                match Self::download_media(_client, image, "image").await {
                    Some(data) => (
                        data,
                        image
                            .mimetype
                            .clone()
                            .unwrap_or_else(|| "image/jpeg".to_string()),
                        false,
                    ),
                    None => (
                        image
                            .jpeg_thumbnail
                            .as_ref()
                            .filter(|t| !t.is_empty())
                            .cloned()
                            .unwrap_or_default(),
                        "image/jpeg".to_string(),
                        true,
                    ),
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
    ) -> tokio::task::JoinHandle<()> {
        let client_handle = self.client_handle.clone();
        let chat_store = self.chat_store.clone();
        let ui_sender = self.ui_sender.clone();
        let jid_str = jid_str.to_string();
        let content = content.to_string();
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
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
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
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
    ) -> tokio::task::JoinHandle<()> {
        let chat_store = self.chat_store.clone();
        let ui_sender = self.ui_sender.clone();
        let client_handle = self.client_handle.clone();
        let jid_str = jid_str.to_string();
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
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
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
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
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
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
    ) -> tokio::task::JoinHandle<()> {
        let client_handle = self.client_handle.clone();
        let chat_jid_str = chat_jid_str.to_string();
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
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
    pub fn mark_status_watched(&self, message_ids: Vec<String>) -> tokio::task::JoinHandle<bool> {
        let chat_store = self.chat_store.clone();
        self.runtime.spawn(async move {
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
    ) -> tokio::task::JoinHandle<()> {
        let client_handle = self.client_handle.clone();
        let chat_jid_str = chat_jid_str.to_string();
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
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

    /// Accept an incoming call: signaling, callKey decrypt, relay connect and
    /// the audio engine are all inside `client.voip().accept(..)`; this side
    /// only supplies the cpal mic/speaker bridge.
    pub fn accept_call(&self, call_id: &str) {
        let client_handle = self.client_handle.clone();
        let calls = self.calls.clone();
        let ui_sender = self.ui_sender.clone();
        let call_id = call_id.to_string();
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            let Some(client) = client_handle.lock().await.clone() else {
                error!("Client not available for accepting call");
                return;
            };
            let Some(offer) = calls.pending.lock().await.remove(&call_id) else {
                warn!("No pending offer for call {}", call_id);
                return;
            };
            let (mic, speaker) = match open_call_audio().await {
                Ok(audio) => audio,
                Err(err) => {
                    error!("Audio device setup failed: {err}");
                    // The offer is consumed and no accept went out: reject
                    // so the caller stops ringing instead of waiting out
                    // the timeout.
                    if let Err(e) = client.voip().reject(&offer).await {
                        error!(
                            "Failed to reject call {} after audio failure: {}",
                            call_id, e
                        );
                    }
                    Self::notify_call_ended(&ui_sender, &call_id).await;
                    return;
                }
            };
            match client
                .voip()
                .accept(&offer)
                .audio(mic, speaker)
                .start()
                .await
            {
                Ok(handle) => {
                    info!("Call {} media live", handle.call_id());
                    let handle = Arc::new(handle);
                    calls
                        .active
                        .lock()
                        .await
                        .insert(call_id.clone(), handle.clone());
                    Self::watch_call_end(handle, calls.clone(), ui_sender.clone());
                }
                Err(e) => {
                    error!("Failed to start call media for {}: {}", call_id, e);
                    Self::notify_call_ended(&ui_sender, &call_id).await;
                }
            }
        });
    }

    /// Decline an incoming call (sends the reject signaling).
    pub fn decline_call(&self, call_id: &str) {
        let client_handle = self.client_handle.clone();
        let calls = self.calls.clone();
        let call_id = call_id.to_string();
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            let Some(client) = client_handle.lock().await.clone() else {
                error!("Client not available for declining call");
                return;
            };
            let Some(offer) = calls.pending.lock().await.remove(&call_id) else {
                warn!("No pending offer for call {}", call_id);
                return;
            };
            match client.voip().reject(&offer).await {
                Ok(()) => info!("Call {} declined", call_id),
                Err(e) => error!("Failed to decline call {}: {}", call_id, e),
            }
        });
    }

    /// Place an outgoing 1:1 voice call. Device discovery, callKey encrypt,
    /// offer send and the relay/engine lifecycle are inside
    /// `client.voip().call(..)`. Video calls are not supported by the library
    /// yet; `is_video` only shapes the UI.
    pub fn start_call(&self, recipient_jid_str: &str, is_video: bool, placeholder_id: String) {
        let client_handle = self.client_handle.clone();
        let calls = self.calls.clone();
        let ui_sender = self.ui_sender.clone();
        let recipient_jid = recipient_jid_str.to_string();
        let runtime = self.runtime.clone();

        if is_video {
            warn!("Video calls are not supported yet; placing a voice call");
        }

        runtime.spawn(async move {
            let notify_failure = |error: String| {
                let ui_sender = ui_sender.clone();
                let recipient_jid = recipient_jid.clone();
                // A cancel may have landed for a call that will never
                // start; consume the marker so the set doesn't grow.
                let calls = calls.clone();
                let placeholder_id = placeholder_id.clone();
                async move {
                    calls.cancelled.lock().await.remove(&placeholder_id);
                    error!(
                        "Failed to start call to {}: {}",
                        observe_str(&recipient_jid),
                        error
                    );
                    if let Some(tx) = ui_sender.lock().await.as_ref() {
                        let _ = tx.send(UiEvent::OutgoingCallFailed {
                            recipient_jid,
                            error,
                        });
                    }
                }
            };

            let jid: Jid = match recipient_jid.parse() {
                Ok(j) => j,
                Err(e) => {
                    notify_failure(format!("invalid JID: {e}")).await;
                    return;
                }
            };
            let Some(client) = client_handle.lock().await.clone() else {
                notify_failure("client not available".to_string()).await;
                return;
            };
            let (mic, speaker) = match open_call_audio().await {
                Ok(audio) => audio,
                Err(err) => {
                    notify_failure(format!("audio device setup failed: {err}")).await;
                    return;
                }
            };

            match client.voip().call(&jid).audio(mic, speaker).start().await {
                Ok(handle) => {
                    let call_id = handle.call_id().to_string();
                    // Cancelled while still connecting: the UI only knew
                    // the placeholder id, so honor it here.
                    if calls.cancelled.lock().await.remove(&placeholder_id) {
                        info!("Outgoing call {} cancelled before start", call_id);
                        // The offer is already out: every device it rang is
                        // ringing, and dropping our side silently would leave
                        // them at it until their own transport gave up.
                        // `terminate` is what tells them, and it tears this
                        // side down whether or not the stanzas landed.
                        log_termination(&call_id, handle.terminate().await);
                        return;
                    }
                    info!(
                        "Outgoing call {} to {} offered",
                        call_id,
                        observe_str(&recipient_jid)
                    );
                    let handle = Arc::new(handle);
                    calls
                        .active
                        .lock()
                        .await
                        .insert(call_id.clone(), handle.clone());
                    Self::watch_call_end(handle, calls.clone(), ui_sender.clone());
                    if let Some(tx) = ui_sender.lock().await.as_ref() {
                        let _ = tx.send(UiEvent::OutgoingCallStarted {
                            call_id,
                            recipient_jid,
                            placeholder_id,
                        });
                    }
                }
                Err(e) => notify_failure(e.to_string()).await,
            }
        });
    }

    /// Hang up / cancel a call we started or answered.
    pub fn cancel_call(&self, call_id: &str) {
        let calls = self.calls.clone();
        let call_id = call_id.to_string();
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            // Still ringing and never answered: nothing live to hang up.
            calls.pending.lock().await.remove(&call_id);
            if let Some(handle) = calls.active.lock().await.remove(&call_id) {
                log_termination(&call_id, handle.terminate().await);
            } else {
                // No handle yet (start_call still connecting under a UI
                // placeholder id): remember the cancel so it lands.
                debug!("cancel_call: no live handle for {}, deferring", call_id);
                calls.cancelled.lock().await.insert(call_id);
            }
        });
    }

    /// Mute or unmute the microphone of a live call, and tell the peer.
    ///
    /// The library commits the two directions around the `<mute_v2>` rather
    /// than at one point — a mute applies before the announcement, an unmute
    /// only once it is out — so whichever half is lost, the microphone is
    /// never live while the peer is being shown a muted one. What that costs
    /// is that a failed announcement leaves the device in a state nobody
    /// asked for, and the front end has already drawn the state it asked for.
    /// So the handle is asked what it really holds and the answer is
    /// published — always, not only when it differs: what makes the state
    /// trustworthy is that the *last* request to reach the device is the one
    /// that speaks last, and a task that only spoke on disagreement would
    /// leave a failed announcement's answer standing over a later success.
    /// It costs nothing, because a call state that does not change sends no
    /// frame.
    ///
    /// The request is stamped here, on the caller's thread, and the work is
    /// what gets spawned — see [`MuteLane`]. A task compares the device
    /// against the *newest* request rather than its own, because its own is
    /// exactly what a superseded task must not restore.
    ///
    /// A call still ringing has nowhere to publish the state, and answering
    /// does not replay it. That is not a gap here: mute is offered on an
    /// active call only ([`oxidezap_core::CallState::set_muted`] matches the
    /// live stage), so nothing can be chosen while it rings.
    pub fn set_call_muted(&self, call_id: &str, muted: bool) {
        let calls = self.calls.clone();
        let ui_sender = self.ui_sender.clone();
        let call_id = call_id.to_string();
        let runtime = self.runtime.clone();

        // Before the spawn, because after it the order is gone.
        let (lane, seq) = {
            let mut lanes = calls.mute.lock().expect("mute lanes poisoned");
            let lane = lanes.entry(call_id.clone()).or_default().clone();
            let mut intent = lane.intent.lock().expect("mute intent poisoned");
            intent.seq += 1;
            intent.muted = muted;
            let seq = intent.seq;
            drop(intent);
            (lane, seq)
        };

        runtime.spawn(async move {
            // Cloned out from under the lock: `set_muted` waits on the call's
            // answer-transition lane, and holding the registry across that
            // would stall every other call's bookkeeping behind one peer.
            let handle = {
                let active = calls.active.lock().await;
                let handle = active.get(&call_id).cloned();
                // Where the lane map grows is where it is swept.
                calls
                    .mute
                    .lock()
                    .expect("mute lanes poisoned")
                    .retain(|id, _| active.contains_key(id));
                handle
            };
            let Some(handle) = handle else {
                debug!("set_call_muted: no live handle for {}", call_id);
                return;
            };

            let _serialized = lane.lane.lock().await;
            // A newer request either has already run or is blocked on the
            // lane behind us; either way it, and not this one, is what the
            // device should end up saying.
            let want = *lane.intent.lock().expect("mute intent poisoned");
            if want.seq != seq {
                return;
            }
            if let Err(e) = handle.set_muted(want.muted).await {
                warn!(
                    "Failed to announce {} on call {}: {}",
                    if want.muted { "mute" } else { "unmute" },
                    call_id,
                    e
                );
            }
            // Superseded while announcing: the request behind us is about to
            // set the state anyway, and a word from here would describe a
            // device that is already on its way somewhere else. It speaks
            // after it has arrived, which is what makes it the last word.
            if lane.intent.lock().expect("mute intent poisoned").seq != seq {
                return;
            }
            // Said whether or not it is news, and this is why. A correction
            // sent only on disagreement is unversioned, and the daemon writes
            // a request's optimistic state before that request is even
            // stamped here — so a *failed* announcement could publish its
            // truth into the window belonging to the retry queued behind it,
            // and the retry, succeeding, would find agreement and say
            // nothing. The state would then hold the failure's answer over
            // the success's device. Speaking unconditionally makes the newest
            // request the one that closes the exchange, and costs nothing:
            // the daemon publishes no frame for a state that did not change.
            let settled = handle.is_muted();
            if let Some(tx) = ui_sender.lock().await.as_ref() {
                let _ = tx.send(UiEvent::CallMuteChanged {
                    call_id,
                    muted: settled,
                });
            }
        });
    }

    /// Watch a live call until it ends (peer hangup, network loss, local
    /// hangup) and clear it from the registry + UI.
    fn watch_call_end(handle: Arc<CallHandle>, calls: CallRegistry, ui_sender: UiEventSender) {
        tokio::spawn(async move {
            handle.wait_ended().await;
            let call_id = handle.call_id().to_string();
            calls.active.lock().await.remove(&call_id);
            // Every call that ever had a handle drains through here, whatever
            // ended it, so this is where a lane is paid for. The sweep in
            // `set_call_muted` is not made redundant by it: a window that fell
            // behind can stamp a request against a call this watcher has
            // already run for, and that lane has no second ending to be
            // removed on.
            calls
                .mute
                .lock()
                .expect("mute lanes poisoned")
                .remove(&call_id);
            Self::notify_call_ended(&ui_sender, &call_id).await;
        });
    }

    async fn notify_call_ended(ui_sender: &UiEventSender, call_id: &str) {
        if let Some(tx) = ui_sender.lock().await.as_ref() {
            let _ = tx.send(UiEvent::CallEnded(call_id.to_string()));
        }
    }
}

/// Say what a hangup achieved.
///
/// The local side is down in every case, so this reports rather than fails: a
/// call the peer was never told about is still over here, and the difference
/// is only how long they keep ringing. A still-ringing call is addressed per
/// device, which is why "some, not all" is one of the answers.
fn log_termination(call_id: &str, outcome: CallTermination) {
    match outcome {
        CallTermination::PeerNotified => info!("Call {} hung up", call_id),
        CallTermination::PartlyNotified {
            notified,
            unconfirmed,
        } => warn!(
            "Call {} hung up; {} device(s) told, {} unconfirmed",
            call_id, notified, unconfirmed
        ),
        CallTermination::LocalOnly(error) => warn!(
            "Call {} hung up locally; the peer was not told: {}",
            call_id, error
        ),
        CallTermination::AlreadyEnded => debug!("Call {} was already over", call_id),
        // `CallTermination` is `#[non_exhaustive]`: a variant added upstream
        // is still an ended call here, and the local side is down in every
        // one of them.
        other => info!("Call {} hung up: {:?}", call_id, other),
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
    const HISTORY_MESSAGES_PER_CHAT: i64 = 50;
    /// Quiet window before reloading: one history-sync chunk commits as many
    /// write batches, each emitting a change; reload once per burst.
    const RELOAD_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

    /// One task for the whole session: chat-store invalidations -> debounced
    /// load_history -> HistoryLoaded. Exits when the store or the UI goes away.
    fn spawn_history_reloader(
        mut changes: tokio::sync::broadcast::Receiver<oxidezap_chat_store::StoreChange>,
        chat_store: Arc<ChatStore>,
        bot: &Bot,
        ui_tx: &mpsc::UnboundedSender<UiEvent>,
        reload: Arc<tokio::sync::Notify>,
        names: Arc<NameBook>,
    ) {
        use tokio::sync::broadcast::error::RecvError;

        let client = bot.client();
        let ui_tx = ui_tx.clone();
        tokio::spawn(async move {
            let mut open = true;
            while open {
                let mut scope = ReloadScope::empty();
                // Either a store change or somebody asking outright. An
                // explicit ask widens to everything, because the asker is a
                // front end that has just attached and holds nothing.
                tokio::select! {
                    change = changes.recv() => match change {
                        Ok(change) => scope.widen(Some(&change)),
                        Err(RecvError::Lagged(_)) => scope.widen(None),
                        Err(RecvError::Closed) => break,
                    },
                    () = reload.notified() => scope.widen(None),
                }
                // Drain the burst; a quiet window flushes the reload.
                loop {
                    match tokio::time::timeout(Self::RELOAD_DEBOUNCE, changes.recv()).await {
                        Ok(Ok(change)) => {
                            scope.widen(Some(&change));
                            continue;
                        }
                        Ok(Err(RecvError::Lagged(_))) => {
                            scope.widen(None);
                            continue;
                        }
                        Ok(Err(RecvError::Closed)) => {
                            // Reload once more: these changes were committed.
                            open = false;
                            break;
                        }
                        Err(_) => break,
                    }
                }
                // An empty COMPLETE load still goes out: the UI prunes
                // against the loaded set, so deleting/archiving the last chat
                // elsewhere must clear the list here too. An empty narrowed
                // one names nothing the list shows (an archived chat, or one
                // past the window) and has nothing to say.
                match Self::load_history_scoped(&chat_store, &client, scope.chats(), &names).await {
                    Ok((chats, complete)) if chats.is_empty() && !complete => {}
                    Ok((chats, complete)) => {
                        if ui_tx
                            .send(UiEvent::HistoryLoaded { chats, complete })
                            .is_err()
                        {
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
    ) -> Result<(Vec<oxidezap_core::Chat>, bool), oxidezap_chat_store::ChatStoreError> {
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
    ) -> Result<(Vec<oxidezap_core::Chat>, bool), oxidezap_chat_store::ChatStoreError> {
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
                let mut page = chat_store
                    .messages(&entry.jid, None, Self::HISTORY_MESSAGES_PER_CHAT)
                    .await?;
                page.reverse();
                if existing.is_status {
                    status_views.extend(watched_ids(&page));
                }
                let mut msgs: Vec<ChatMessage> =
                    page.into_iter().map(stored_to_chat_message).collect();
                Self::hydrate_reactions(chat_store, client, names, &entry.jid, &mut msgs).await;
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
                let mut remaining = entry.unread_count.max(0) as u32;
                for msg in msgs.iter_mut().rev() {
                    if remaining == 0 {
                        break;
                    }
                    if !msg.is_from_me {
                        msg.is_read = false;
                        remaining -= 1;
                    }
                }
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

            let mut page = chat_store
                .messages(&entry.jid, None, Self::HISTORY_MESSAGES_PER_CHAT)
                .await?;
            page.reverse(); // store returns newest-first; the UI renders oldest-first
            if chat.is_status {
                status_views.extend(watched_ids(&page));
            }
            chat.messages = page.into_iter().map(stored_to_chat_message).collect();
            Self::hydrate_reactions(chat_store, client, names, &entry.jid, &mut chat.messages)
                .await;
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
            // The newest `unread_count` incoming messages are the unread ones;
            // select_chat only sends read receipts for !is_read, so hydrated
            // unread must not come up pre-read.
            let mut remaining = chat.unread_count;
            for msg in chat.messages.iter_mut().rev() {
                if remaining == 0 {
                    break;
                }
                if !msg.is_from_me {
                    msg.is_read = false;
                    remaining -= 1;
                }
            }
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
        Ok((chats, complete))
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
        for msg in msgs.iter_mut() {
            let entries = match chat_store.reactions(chat_jid, &msg.id).await {
                Ok(entries) => entries,
                Err(e) => {
                    warn!("failed to hydrate reactions for {}: {e}", msg.id);
                    continue;
                }
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
            for (sender, emoji) in latest {
                msg.reactions.entry(emoji).or_default().push(sender);
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
        let thumbnail = sticker
            .png_thumbnail
            .as_ref()
            .filter(|thumbnail| !thumbnail.is_empty())
            .cloned()
            .unwrap_or_default();
        if thumbnail.is_empty() && downloadable.is_none() {
            return None;
        }
        let has_preview = !thumbnail.is_empty();
        return Some(MediaContent {
            media_type: MediaType::Sticker,
            data: Arc::new(thumbnail),
            cache_key: None,
            mime_type: if has_preview {
                "image/png".to_string()
            } else {
                mime
            },
            width: sticker.width,
            height: sticker.height,
            caption: None,
            file_name: None,
            data_is_preview: has_preview && downloadable.is_some(),
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
        let thumbnail = image
            .jpeg_thumbnail
            .as_ref()
            .filter(|t| !t.is_empty())
            .cloned()
            .unwrap_or_default();
        if thumbnail.is_empty() && downloadable.is_none() {
            return None;
        }
        // Hydrated rows carry only the thumbnail; flag it so the renderer
        // keeps offering the full download instead of treating it as final
        let data_is_preview = !thumbnail.is_empty() && downloadable.is_some();
        return Some(MediaContent {
            media_type: MediaType::Image,
            data: Arc::new(thumbnail),
            cache_key: None,
            mime_type: "image/jpeg".to_string(),
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
        // its background thread instead of leaking the runtime + DB pool.
        self.shutdown.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        ChatStore, Client, MuteLane, NameBook, ReadBoundary, ReloadScope, SqliteStore, StoreChange,
        WhatsAppClient, apply_status_views, media_metadata, merge_alias_history_messages,
        read_message_range,
    };
    use oxidezap_core::{Chat, ChatMessage, MessageStatus, fallback_chat_name};
    use std::sync::Arc;
    use whatsapp_rust::buffa::MessageField;
    use whatsapp_rust::wacore::proto_helpers::MessageBuilderExt;
    use whatsapp_rust::wacore_binary::Jid;
    use whatsapp_rust::waproto::whatsapp as wa;

    /// Stamp a request the way `set_call_muted` does, on the caller's thread.
    fn request(lane: &MuteLane, muted: bool) -> u64 {
        let mut intent = lane.intent.lock().unwrap();
        intent.seq += 1;
        intent.muted = muted;
        intent.seq
    }

    /// Two toggles in quick succession are spawned as two tasks, and spawn
    /// order is not run order. Run the wrong way round, each task saw the
    /// device holding the value it had itself asked for and corrected
    /// nothing — so an unmute that executed last left the microphone open
    /// under a state, and every window, still showing it muted.
    ///
    /// The order survives because it is stamped before the tasks exist, and a
    /// task that is no longer the newest does nothing at all.
    #[test]
    fn only_the_newest_mute_request_reaches_the_device() {
        let lane = MuteLane::default();
        // Muted, and the user changes their mind twice.
        let unmute = request(&lane, false);
        let remute = request(&lane, true);

        // Whichever task wins the lane, the gate answers the same way.
        let newest = *lane.intent.lock().unwrap();
        assert_ne!(unmute, remute);
        assert_eq!(newest.seq, remute, "the last request is the live one");
        assert!(newest.muted, "and it is the one the device must end on");
        assert_ne!(
            newest.seq, unmute,
            "the superseded task yields instead of restoring its own value"
        );
    }

    /// A lone request is nobody's stale task: it applies, and it is the one
    /// that answers for what the device really did.
    #[test]
    fn a_single_mute_request_is_the_newest_one() {
        let lane = MuteLane::default();
        let seq = request(&lane, true);
        let newest = *lane.intent.lock().unwrap();
        assert_eq!(newest.seq, seq);
        assert!(newest.muted);
    }

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

        let (chats, complete) = WhatsAppClient::load_history(&chat_store, &client, &book())
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

        let (chats, _) = WhatsAppClient::load_history(&chat_store, &client, &book())
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

        let (chats, _) = WhatsAppClient::load_history(&chat_store, &client, &book())
            .await
            .expect("history loads");
        assert_eq!(chats[0].last_message.as_deref(), Some("bom dia"));
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
        use whatsapp_rust::wacore::types::events::{
            BatchOrigin, Event, InboundMessage, MessageBatch,
        };
        use whatsapp_rust::wacore::types::message::{MessageInfo, MessageSource};

        let info = MessageInfo {
            source: MessageSource {
                chat: TEST_PEER.parse().expect("test JID"),
                sender: TEST_PEER.parse().expect("test JID"),
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
        let (chats, complete) =
            WhatsAppClient::load_history_scoped(&chat_store, &client, Some(&only), &book())
                .await
                .expect("history loads");

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
}
