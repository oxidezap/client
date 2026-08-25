//! Main WhatsApp application state and logic
//!
//! This module is being refactored into submodules for better organization:
//! - `chats`: Chat list management, selection, search, keyboard navigation
//! - `media`: Media handling (PTT recording state)
//! - `messages`: Message list caching and height calculation
//! - `calls`: Call state management (incoming/outgoing)

mod calls;
mod calls_ctl;
pub mod chat_row;
mod chats;
mod commands;
mod events;
mod media;
mod media_ctl;
mod messages;
mod recording;
mod recovery;
mod settings;
mod timeline_ctl;

pub use calls::{ActiveCall, CallState as CallStateMachine, Stage};
pub use chat_row::{ChatRow, Preview, PreviewGlyph, Unread};
pub use chats::{ChatFilter, ChatListCache};
pub use media::RecordingState;
pub use messages::{MessageListCache, TimelineItem};
pub use settings::{SettingsSection, SettingsState};

use calls::CallState;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use indexmap::IndexMap;

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, Image, KeyBinding, ScrollStrategy, Task,
    WeakEntity, Window, actions, div, prelude::*,
};
use gpui_component::VirtualListScrollHandle;
use gpui_component::input::InputState;

// Define our own actions since gpui-component's actions module is private
actions!(chat_list, [SelectUp, SelectDown]);
actions!(
    oxidezap,
    [
        /// Move focus to the conversation search field.
        FocusSearch,
        /// Open the Settings screen.
        OpenSettings,
        /// Dismiss the topmost overlay: Settings, the media viewer, a reply.
        CloseOverlay,
        /// Mute or unmute the microphone of the live call.
        ToggleMute,
        /// Bring a minimised call card back to full size.
        ReturnToCall,
    ]
);

use crate::components::{AccountSummary, InputAreaEvent, InputAreaView, ReplyDraft};
use log::{debug, error, info, warn};
use whatsapp_rust::wacore_binary::jid::{Jid, JidExt, observe_str};

use crate::responsive::{MobilePanel, ResponsiveLayout};
use crate::session::Session;
use crate::theme::ActiveProductTheme as _;
use crate::utils::mime_to_image_format;
use crate::video::{StreamingVideoDecoder, VideoPlayer, VideoPlayerState};
use crate::views::pairing::generate_qr_png;
use crate::views::{
    render_connected_view, render_connecting_view, render_error_view, render_loading_view,
    render_logged_out_view, render_pairing_view, render_settings_view, render_syncing_view,
};
use oxidezap_audio::{AudioPlayer, AudioRecorder, encode_to_opus_ogg, generate_waveform};
use oxidezap_core::{
    AppState, Availability, CachedQrCode, CallOutcome, CallRecord, Chat, ChatMessage,
    ComposingKind, DownloadableMedia, IncomingCall, MediaContent, MediaType, MessageStatus,
    OutgoingCall, PresenceRegistry, ReceiptType, SystemNotice, TypingSummary, UiEvent,
};

// ChatListCache is now in chats.rs and re-exported above
// RecordingState is now in media/mod.rs and re-exported above
// MessageListCache is now in messages.rs and re-exported above

/// Key context for chat list keyboard navigation
const CHAT_LIST_CONTEXT: &str = "ChatList";

/// Key context for the call card. Scoped rather than global so Enter/Escape
/// keep their meaning in the composer while no call is up.
pub const CALL_CONTEXT: &str = "Call";

/// Search debounce delay in milliseconds
const SEARCH_DEBOUNCE_MS: u64 = 150;

/// Maximum number of video players to keep cached (each holds decoded frames)
const MAX_VIDEO_PLAYERS: usize = 10;

/// Maximum number of sticker images to keep cached
const MAX_DECODED_IMAGES: usize = 50;

/// Download timeout in seconds (for audio/video downloads)
const DOWNLOAD_TIMEOUT_SECS: u64 = 60;

/// Download media with timeout - returns Ok(data) or Err(error message)
async fn download_with_timeout(
    download_rx: tokio::sync::oneshot::Receiver<Result<Vec<u8>, String>>,
) -> Result<Vec<u8>, String> {
    let timeout = smol::Timer::after(std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS));
    let download = async {
        download_rx
            .await
            .unwrap_or(Err("Download cancelled".to_string()))
    };

    // Race between download and timeout
    smol::future::or(async { Some(download.await) }, async {
        timeout.await;
        None
    })
    .await
    .ok_or_else(|| "Download timed out".to_string())?
}

/// Write a downloaded document into the user's Downloads directory
/// ($XDG_DOWNLOAD_DIR, then $HOME or %USERPROFILE% + /Downloads, then the CWD
/// like the database fallback when no home is known) and return the path
/// written.
fn save_to_downloads(file_name: &str, data: &[u8]) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    use std::path::PathBuf;

    let not_empty = |v: std::ffi::OsString| (!v.is_empty()).then_some(PathBuf::from(v));
    let dir = std::env::var_os("XDG_DOWNLOAD_DIR")
        .and_then(not_empty)
        .or_else(|| {
            std::env::var_os("HOME")
                .and_then(not_empty)
                .or_else(|| std::env::var_os("USERPROFILE").and_then(not_empty))
                .map(|home| home.join("Downloads"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;

    // The name comes off the wire: strip path separators (and `:`, which on
    // Windows makes a drive-relative path) so a hostile sender can't traverse
    // out of the directory.
    let sanitized: String = file_name
        .chars()
        .map(|c| {
            if std::path::is_separator(c) || c == '\\' || c == ':' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let name = match sanitized.trim() {
        "" | "." | ".." => "document",
        trimmed => trimmed,
    };

    // Windows treats device basenames (CON, NUL, COM1…) as reserved for any
    // extension; prefix them so the save can't resolve to a device.
    let stem = name
        .split_once('.')
        .map_or(name, |(stem, _)| stem)
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    let name = if reserved {
        format!("_{name}")
    } else {
        name.to_string()
    };

    // create_new + " (n)" suffixing so a download never clobbers an existing
    // file of the same name.
    for attempt in 0..1000u32 {
        let candidate = if attempt == 0 {
            name.to_string()
        } else {
            match name.rsplit_once('.') {
                Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({attempt}).{ext}"),
                _ => format!("{name} ({attempt})"),
            }
        };
        let path = dir.join(candidate);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(data)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "too many downloads with the same name",
    ))
}

/// Currently active media playback (mutual exclusion: only one media at a time)
/// Animated stickers are excluded from this - they can play alongside audio/video.
#[derive(Clone, Debug, Default)]
enum ActiveMedia {
    /// No media currently playing
    #[default]
    None,
    /// Audio message playing (voice message, PTT)
    Audio { message_id: String },
    /// Video message playing (includes video's audio track)
    Video { message_id: String },
}

impl ActiveMedia {
    /// Check if this is an audio message
    fn is_audio(&self) -> bool {
        matches!(self, Self::Audio { .. })
    }

    /// Check if this is a video message
    fn is_video(&self) -> bool {
        matches!(self, Self::Video { .. })
    }

    /// Get the message ID if any media is playing
    fn message_id(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Audio { message_id } | Self::Video { message_id } => Some(message_id),
        }
    }

    /// Check if the given message ID is currently playing
    fn is_playing(&self, id: &str) -> bool {
        self.message_id() == Some(id)
    }
}

// Answering a call must not require a pointer.
actions!(calls, [AcceptCall, DeclineCall]);

/// Initialize chat list and call popup key bindings
pub fn init_app_bindings(cx: &mut gpui::App) {
    // `secondary` is Command on macOS and Control elsewhere, which is what
    // makes one binding table correct on every platform.
    cx.bind_keys([
        KeyBinding::new("up", SelectUp, Some(CHAT_LIST_CONTEXT)),
        KeyBinding::new("down", SelectDown, Some(CHAT_LIST_CONTEXT)),
        // Window-wide: reachable whatever owns focus, because both are ways
        // *out* of wherever the user currently is.
        KeyBinding::new("secondary-k", FocusSearch, None),
        KeyBinding::new("secondary-,", OpenSettings, None),
        // Scoped to the call so Enter and Escape keep their composer meaning
        // while nothing is ringing.
        KeyBinding::new("enter", AcceptCall, Some(CALL_CONTEXT)),
        KeyBinding::new("escape", DeclineCall, Some(CALL_CONTEXT)),
        KeyBinding::new("secondary-shift-m", ToggleMute, Some(CALL_CONTEXT)),
        KeyBinding::new("secondary-shift-c", ReturnToCall, None),
        // Dismissing the topmost surface is a window-level command; each
        // overlay decides in turn whether it is the one that closes.
        KeyBinding::new("escape", CloseOverlay, None),
    ]);
}

// Action to navigate back to chat list on mobile
actions!(mobile_nav, [NavigateBack]);

/// Why a chat is being opened, which decides where keyboard focus lands.
///
/// Clicking a chat means "I want to talk to this person", so the composer
/// takes focus and typing just works. Arrow-key navigation means "show me
/// this one" — moving focus to the composer there would take the arrow keys
/// away from the list the user is still walking through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatOpen {
    ToCompose,
    ToPreview,
}

/// Main application struct
pub struct WhatsAppApp {
    /// Current application state
    app_state: AppState,
    /// List of chats
    chats: Vec<Chat>,
    /// Currently selected chat JID
    selected_chat: Option<String>,
    /// WhatsApp client wrapper
    client: Option<Session>,
    /// Scroll handle for chat list
    chat_list_scroll: VirtualListScrollHandle,
    /// Focus handle for chat list keyboard navigation
    chat_list_focus: FocusHandle,
    /// Focus target for the call card, so its actions are reachable from
    /// the keyboard while it floats over the app.
    call_focus: FocusHandle,
    /// Search input state for chat list (created lazily when window is available)
    chat_search_input: Option<Entity<InputState>>,
    /// Current search query (lowercase, trimmed)
    chat_search_query: String,
    /// Debounced search task
    #[allow(dead_code)]
    chat_search_task: Option<Task<()>>,
    /// Scroll handle for message list
    message_list_scroll: VirtualListScrollHandle,
    /// Isolated input area view (has its own render cycle for performance)
    input_area: Option<Entity<InputAreaView>>,
    /// Chat a composing indicator was last sent to: paused must go back to
    /// this chat even if the user switched chats before the typing timeout
    composing_chat: Option<String>,
    /// Unsent input text stashed per chat on switch; the shared input view
    /// would otherwise carry chat A's draft into chat B and send it there
    drafts: HashMap<String, String>,
    /// Background task for event polling (must be retained)
    #[allow(dead_code)]
    event_task: Option<Task<()>>,
    /// Reconnecting to the daemon, which can mean starting one and waiting
    /// for it to listen. Retained for the same reason: dropping the task
    /// cancels it.
    #[allow(dead_code)]
    reconnect_task: Option<Task<()>>,
    /// Audio recorder for PTT messages
    audio_recorder: AudioRecorder,
    /// Current recording state
    recording_state: RecordingState,
    /// Chat the current PTT recording started in; the note is sent there even
    /// if the user switches chats before stopping
    recording_chat: Option<String>,
    /// Audio player for voice message and video audio playback
    audio_player: AudioPlayer,
    /// Playback speed for voice notes, shared across clips: someone who
    /// listens at 1.5× means it for the next note too.
    playback_speed: f32,
    /// Repaints the playhead while audio plays. Only alive while it does.
    #[allow(dead_code)]
    playback_tick: Option<Task<()>>,
    /// Message ID of the audio currently loaded in audio_player (for ownership tracking)
    /// This ensures we don't resume audio from a different video when switching
    audio_owner: Option<String>,
    /// Currently active media (mutual exclusion: only one audio or video at a time)
    active_media: ActiveMedia,
    /// Message id of the most recent user-requested playback; download/decode
    /// completions autoplay only if they still match it, so a stale download
    /// can't steal playback from media the user started meanwhile.
    pending_media_request: Option<String>,
    /// When the automatic retry fires, while the error screen is up.
    retry_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Counts the retry down. Only alive while the error screen is.
    #[allow(dead_code)]
    retry_task: Option<Task<()>>,
    /// Whether the error screen's technical detail is unfolded.
    error_detail_open: bool,
    /// Message ids whose media is being fetched right now, so a bubble can
    /// say so and a second tap cannot start the same download twice.
    downloads_in_flight: std::collections::HashSet<String>,
    /// Call state (incoming and outgoing calls)
    call_state: CallState,
    /// Cache of JID -> display name mappings (from notify/pushname attribute)
    name_cache: HashMap<String, String>,
    /// Video players for each message (message_id -> VideoPlayer)
    video_players: HashMap<String, VideoPlayer>,
    /// Task for video frame updates
    #[allow(dead_code)]
    video_update_task: Option<Task<()>>,
    /// Cache of decoded images (message_id -> Arc<Image>): sticker animation
    /// state and per-render decode cost both depend on the Arc being stable.
    /// Uses RefCell for interior mutability since we need to cache during immutable render.
    /// Uses IndexMap to maintain insertion order for deterministic FIFO eviction.
    decoded_images: RefCell<IndexMap<String, Arc<Image>>>,
    /// Cache of message list data per chat to avoid expensive recomputation on every render.
    /// Key is the chat JID, value is the cached data.
    message_list_cache: RefCell<HashMap<String, MessageListCache>>,
    /// Cache of chat list data to avoid recomputation on every render.
    chat_list_cache: RefCell<Option<ChatListCache>>,
    /// Mobile navigation state - which panel to show on mobile devices
    mobile_panel: MobilePanel,
    /// Which conversations the sidebar is showing.
    chat_filter: ChatFilter,
    /// The message being replied to, mirrored here so the send path can
    /// attach it and the composer can show it.
    reply_to: Option<ReplyDraft>,
    /// Who is typing and who is around. Expires on its own, so it is view
    /// state rather than anything the store carries.
    presence: PresenceRegistry,
    /// Display name of the linked account, for the sidebar footer.
    account_name: Option<String>,
    /// The Settings screen, when it is open. `None` is the conversation view.
    settings: Option<SettingsState>,
    /// Repaints the call duration, and expires stale typing notices. Only
    /// alive while there is something to tick.
    #[allow(dead_code)]
    tick_task: Option<Task<()>>,
}

impl WhatsAppApp {
    /// Spawn the event handling task that processes UI events from the WhatsApp client
    fn spawn_event_task(
        mut ui_rx: tokio::sync::mpsc::Receiver<UiEvent>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            while let Some(event) = ui_rx.recv().await {
                let result = entity.update(cx, |app, cx| {
                    app.handle_event(event, cx);
                });
                if result.is_err() {
                    // Entity was dropped, stop the loop
                    break;
                }
            }
        })
    }

    /// Create a new WhatsApp application
    pub fn new(cx: &mut Context<Self>) -> Self {
        let bootstrap = Session::connect();
        let (app_state, client, event_task) = match bootstrap {
            Ok((client, ui_rx)) => (
                AppState::Loading,
                Some(client),
                Some(Self::spawn_event_task(ui_rx, cx)),
            ),
            Err(e) => (
                AppState::Error(format!("Failed to reach the daemon: {e}")),
                None,
                None,
            ),
        };

        Self {
            app_state,
            chats: Vec::new(),
            selected_chat: None,
            client,
            chat_list_scroll: VirtualListScrollHandle::new(),
            chat_list_focus: cx.focus_handle(),
            call_focus: cx.focus_handle(),
            chat_search_input: None, // Created lazily when window is available
            chat_search_query: String::new(),
            chat_search_task: None,
            message_list_scroll: VirtualListScrollHandle::new(),
            input_area: None,
            composing_chat: None,
            drafts: HashMap::new(),
            event_task,
            reconnect_task: None,
            audio_recorder: AudioRecorder::new(),
            recording_state: RecordingState::default(),
            recording_chat: None,
            audio_player: AudioPlayer::new(),
            playback_speed: 1.0,
            playback_tick: None,
            audio_owner: None,
            active_media: ActiveMedia::None,
            pending_media_request: None,
            retry_at: None,
            retry_task: None,
            error_detail_open: false,
            downloads_in_flight: std::collections::HashSet::new(),
            call_state: CallState::new(),
            name_cache: HashMap::new(),
            video_players: HashMap::new(),
            video_update_task: None,
            decoded_images: RefCell::new(IndexMap::new()),
            message_list_cache: RefCell::new(HashMap::new()),
            chat_list_cache: RefCell::new(None),
            mobile_panel: MobilePanel::default(),
            chat_filter: ChatFilter::default(),
            reply_to: None,
            presence: PresenceRegistry::new(),
            account_name: None,
            settings: None,
            tick_task: None,
        }
    }

    // ========== Responsive Layout ==========

    /// The layout facts for this frame: the viewport, the mobile panel, and
    /// the active design scale.
    ///
    /// Read once at the top of render and threaded down, so every component in
    /// a frame agrees on the same breakpoint and the same metrics.
    pub fn responsive_layout(&self, window: &Window, cx: &App) -> ResponsiveLayout {
        ResponsiveLayout::new(
            window.viewport_size(),
            self.mobile_panel,
            cx.product().metrics,
        )
    }

    /// Get the current mobile panel state
    pub fn mobile_panel(&self) -> MobilePanel {
        self.mobile_panel
    }

    /// Navigate back to chat list (for mobile)
    pub fn navigate_back(&mut self, cx: &mut Context<Self>) {
        self.mobile_panel = MobilePanel::ChatList;
        cx.notify();
    }

    /// Navigate to chat view (for mobile) - called when selecting a chat
    fn navigate_to_chat(&mut self) {
        self.mobile_panel = MobilePanel::Chat;
    }

    // ========== Render Caches ==========

    /// Get or compute the chat list cache.
    /// This avoids expensive recomputation of chat list data on every render.
    /// Filters by search query if active. Item sizes are computed at render time
    /// based on ResponsiveLayout.
    pub fn get_chat_list_cache(&self) -> ChatListCache {
        let mut cache = self.chat_list_cache.borrow_mut();

        let query = &self.chat_search_query;
        let filtered: Vec<&Chat> = self
            .chats
            .iter()
            .filter(|chat| self.chat_filter.matches(chat))
            .filter(|chat| {
                query.is_empty()
                    || chat.name.to_lowercase().contains(query)
                    || chat.jid.to_lowercase().contains(query)
            })
            .collect();

        // The count alone cannot see a preview change — a receipt, a draft, a
        // typing notice — so every path that changes one invalidates the cache
        // explicitly. This guard only skips the rebuild when nothing was
        // added or removed *and* nothing claimed a change.
        if let Some(cached) = cache.as_ref()
            && cached.chat_count == filtered.len()
        {
            return cached.clone();
        }

        let rows: Arc<[ChatRow]> = filtered
            .into_iter()
            .map(|chat| {
                ChatRow::new(
                    chat,
                    self.presence.typing(&chat.jid),
                    // The open chat's text lives in the composer, not the
                    // draft map, so it would otherwise show as a stale draft.
                    (self.selected_chat.as_deref() != Some(chat.jid.as_str()))
                        .then(|| self.drafts.get(&chat.jid).map(String::as_str))
                        .flatten(),
                )
            })
            .collect();

        let new_cache = ChatListCache {
            chat_count: rows.len(),
            rows,
        };

        *cache = Some(new_cache.clone());
        new_cache
    }

    /// How many conversations carry unread state, for the filter chip.
    ///
    /// Counted over every chat rather than the filtered view: the number has
    /// to say what pressing `Unread` would reveal.
    pub fn unread_chat_count(&self) -> usize {
        self.chats
            .iter()
            .filter(|chat| ChatFilter::Unread.matches(chat))
            .count()
    }

    pub fn chat_filter(&self) -> ChatFilter {
        self.chat_filter
    }

    /// Narrow the list. Changing the filter re-derives the rows, so the cache
    /// has to go with it.
    pub fn set_chat_filter(&mut self, filter: ChatFilter, cx: &mut Context<Self>) {
        if self.chat_filter == filter {
            return;
        }
        self.chat_filter = filter;
        self.invalidate_chat_cache();
        cx.notify();
    }

    /// Whether a search is currently narrowing the list, which is what makes
    /// an empty list mean "no matches" rather than "no chats".
    pub fn is_searching(&self) -> bool {
        !self.chat_search_query.is_empty()
    }

    /// The linked-device row at the foot of the sidebar.
    pub fn account_summary(&self) -> Option<AccountSummary> {
        let connected = matches!(self.app_state, AppState::Connected);
        Some(AccountSummary {
            name: self.account_name.clone()?,
            status: if connected {
                "linked device · synced".to_string()
            } else {
                "linked device · reconnecting".to_string()
            },
            is_healthy: connected,
        })
    }

    /// Invalidate chat list cache (call when chats change or search changes)
    fn invalidate_chat_cache(&self) {
        *self.chat_list_cache.borrow_mut() = None;
    }

    // ========== Message List Cache ==========

    /// Get or compute the message list cache for a chat.
    /// This avoids expensive recomputation of message heights on every render.
    /// Uses interior mutability so it can be called during immutable render.
    /// `max_media_size` should come from ResponsiveLayout for correct sizing.
    pub fn get_message_list_cache(
        &self,
        chat_jid: &str,
        messages: &[ChatMessage],
        is_group: bool,
        max_media_size: f32,
        metrics: crate::theme::Metrics,
        typing: Option<TypingSummary>,
    ) -> MessageListCache {
        let mut cache = self.message_list_cache.borrow_mut();

        // Heights are resolved geometry, so every input that moves them —
        // the viewport, the group flag, the base font, the density, and
        // whether a typing row is present — is part of the key.
        if let Some(cached) = cache.get(chat_jid)
            && cached.is_valid_for(
                messages.len(),
                is_group,
                max_media_size,
                metrics,
                typing.is_some(),
            )
        {
            return cached.clone();
        }

        let new_cache = MessageListCache::new(messages, is_group, max_media_size, metrics, typing);
        cache.insert(chat_jid.to_string(), new_cache.clone());
        new_cache
    }

    /// Invalidate message list cache for a chat (call when messages change)
    fn invalidate_message_cache(&self, chat_jid: &str) {
        self.message_list_cache.borrow_mut().remove(chat_jid);
    }

    // ========== Accessors ==========

    /// Check if the client is connected
    fn is_connected(&self) -> bool {
        matches!(self.app_state, AppState::Connected)
    }

    /// Get the selected chat JID
    pub fn selected_chat_jid(&self) -> Option<String> {
        self.selected_chat.clone()
    }

    /// Get the currently selected chat data
    pub fn selected_chat_data(&self) -> Option<&Chat> {
        self.selected_chat
            .as_ref()
            .and_then(|jid| self.find_chat(jid))
    }

    /// Find a chat by JID (immutable)
    fn find_chat(&self, jid: &str) -> Option<&Chat> {
        self.chats.iter().find(|c| c.jid == jid)
    }

    /// Find a chat by JID (mutable)
    fn find_chat_mut(&mut self, jid: &str) -> Option<&mut Chat> {
        self.chats.iter_mut().find(|c| c.jid == jid)
    }

    /// Add a message to a chat, bumping it to the top of the list only when
    /// the message actually advances the chat (duplicates and older backfills
    /// leave the ordering alone).
    /// Returns true if the chat was found and updated, false otherwise.
    fn add_message_to_chat(&mut self, jid: &str, message: ChatMessage) -> bool {
        if let Some(index) = self.chats.iter().position(|c| c.jid == jid) {
            if self.chats[index].add_message(message) {
                self.move_chat_to_top(index);
            }
            // Always invalidate chat cache since the chat's content changed
            // (even if it didn't move, the last message preview needs updating)
            self.invalidate_chat_cache();
            // Also invalidate message cache for this chat
            self.invalidate_message_cache(jid);
            true
        } else {
            false
        }
    }

    /// Move a chat at the given index to the top of the list (index 0).
    /// Does nothing if already at top.
    fn move_chat_to_top(&mut self, index: usize) {
        if index > 0 && index < self.chats.len() {
            let chat = self.chats.remove(index);
            self.chats.insert(0, chat);
            // Note: chat cache invalidation is handled by the caller
        }
    }

    /// Get the chat list scroll handle
    pub fn chat_list_scroll(&self) -> &VirtualListScrollHandle {
        &self.chat_list_scroll
    }

    /// Get the message list scroll handle
    pub fn message_list_scroll(&self) -> &VirtualListScrollHandle {
        &self.message_list_scroll
    }

    /// Scroll to the last message in the currently selected chat.
    /// Uses scroll_to_item with the actual message count (not scroll_to_bottom,
    /// which relies on internal state that may be stale before paint).
    fn scroll_to_last_message(&self) {
        if let Some(ref jid) = self.selected_chat
            && let Some(chat) = self.find_chat(jid)
            && !chat.messages.is_empty()
        {
            self.message_list_scroll
                .scroll_to_item(chat.messages.len() - 1, ScrollStrategy::Top);
        }
    }

    /// Get the isolated input area view entity
    pub fn input_area(&self) -> Option<Entity<InputAreaView>> {
        self.input_area.clone()
    }

    /// Get the chat list focus handle
    pub fn call_popup_focus(&self) -> &FocusHandle {
        &self.call_focus
    }

    pub fn chat_list_focus(&self) -> &FocusHandle {
        &self.chat_list_focus
    }

    /// Get the chat search input entity
    pub fn chat_search_input(&self) -> Option<&Entity<InputState>> {
        self.chat_search_input.as_ref()
    }

    /// Ensure the chat search input is initialized
    pub fn ensure_chat_search_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_component::input::InputEvent;

        if self.chat_search_input.is_some() {
            return;
        }

        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("Search chats..."));

        // Subscribe to search input changes
        cx.subscribe(&search_input, |this, input, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                let query = input.read(cx).value().to_string();
                this.set_chat_search(query, cx);
            }
        })
        .detach();

        self.chat_search_input = Some(search_input);
    }

    // ========== Chat List Navigation ==========

    /// Select the next chat in the list (keyboard navigation)
    pub fn select_next_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cache = self.get_chat_list_cache();
        if cache.rows.is_empty() {
            return;
        }

        let current_index = self
            .selected_chat
            .as_ref()
            .and_then(|jid| cache.rows.iter().position(|row| &row.jid == jid));

        let next_index = match current_index {
            Some(idx) if idx + 1 < cache.rows.len() => idx + 1,
            None => 0,
            _ => return, // Already at the end
        };

        let next_jid = cache.rows[next_index].jid.clone();
        self.select_chat(next_jid, ChatOpen::ToPreview, window, cx);
        self.chat_list_scroll
            .scroll_to_item(next_index, ScrollStrategy::Top);
    }

    /// Select the previous chat in the list (keyboard navigation)
    pub fn select_previous_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cache = self.get_chat_list_cache();
        if cache.rows.is_empty() {
            return;
        }

        let current_index = self
            .selected_chat
            .as_ref()
            .and_then(|jid| cache.rows.iter().position(|row| &row.jid == jid));

        let prev_index = match current_index {
            Some(idx) if idx > 0 => idx - 1,
            None => cache.rows.len() - 1,
            _ => return, // Already at the beginning
        };

        let prev_jid = cache.rows[prev_index].jid.clone();
        self.select_chat(prev_jid, ChatOpen::ToPreview, window, cx);
        self.chat_list_scroll
            .scroll_to_item(prev_index, ScrollStrategy::Top);
    }

    // ========== Chat Search ==========

    /// Update chat search query with debouncing
    pub fn set_chat_search(&mut self, query: String, cx: &mut Context<Self>) {
        // Cancel previous debounce task
        self.chat_search_task = None;

        if query.is_empty() {
            // Immediate clear
            self.chat_search_query.clear();
            self.invalidate_chat_cache();
            cx.notify();
            return;
        }

        // Debounce the actual filtering
        let trimmed = query.trim().to_lowercase();
        self.chat_search_task = Some(cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(SEARCH_DEBOUNCE_MS))
                .await;

            let _ = entity.update(cx, |this, cx| {
                this.chat_search_query = trimmed;
                this.invalidate_chat_cache();
                cx.notify();
            });
        }));
    }

    // ========== Actions ==========

    pub fn select_chat(
        &mut self,
        jid: String,
        open: ChatOpen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_current_media();
        // Leaving a chat mid-composition: release its typing indicator now,
        // or it would stay "typing..." and the eventual paused would land on
        // the newly selected chat instead.
        if self.composing_chat.as_deref() != Some(jid.as_str())
            && let Some(prev) = self.composing_chat.take()
        {
            if let Some(client) = &self.client {
                client.send_paused(&prev);
            }
            if let Some(ref input_area) = self.input_area {
                input_area.update(cx, |view, _| view.reset_typing());
            }
        }
        // Stash the outgoing chat's unsent text and restore the target's, or
        // the shared input would send A's draft to B. Skipped on reselect so
        // in-progress text survives a same-chat click.
        if self.selected_chat.as_deref() != Some(jid.as_str())
            && let Some(ref input_area) = self.input_area
        {
            let restored = self.drafts.remove(&jid).unwrap_or_default();
            let old = input_area.update(cx, |view, cx| view.swap_text(&restored, window, cx));
            if let Some(prev) = self.selected_chat.clone()
                && !old.trim().is_empty()
            {
                self.drafts.insert(prev, old);
            }
        }
        self.selected_chat = Some(jid.clone());
        self.navigate_to_chat();

        if open == ChatOpen::ToCompose {
            // After `navigate_to_chat`, so on mobile the composer exists on the
            // panel being switched to rather than the one being left.
            self.ensure_input_area(window, cx);
            if let Some(input_area) = self.input_area.clone() {
                // Read the handle out before focusing: `focus` needs `&mut App`
                // and `read` holds `cx` borrowed for as long as its result lives.
                let handle = input_area.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
        }

        // One request, where this used to send receipts and a bounded chat
        // action separately: the daemon owns both, along with the boundary
        // that keeps a read from swallowing anything newer. All it needs from
        // here is the message this side is looking at.
        if let Some(chat) = self
            .find_chat(&jid)
            .filter(|c| c.unread_count > 0 || c.manually_unread)
        {
            info!("Marking {} read", observe_str(&jid));
            let newest = chat.messages.last().map(|m| m.id.clone());
            if let Some(client) = &self.client {
                client.mark_chat_read(&jid, newest);
            }
        }

        // Mark as read locally
        if let Some(chat) = self.find_chat_mut(&jid) {
            chat.mark_as_read();
            // Both caches: the badge, and the is_read snapshot the message
            // list renders ticks from (its count guard can't see this).
            self.invalidate_chat_cache();
            self.invalidate_message_cache(&jid);
        }

        // Scroll to the last message
        self.scroll_to_last_message();
        cx.notify();
    }

    /// Retry connection after an error
    /// Drop the dead session and start over at pairing.
    ///
    /// Split from [`retry_connection`](Self::retry_connection) because the
    /// server rejected the stored credentials: reconnecting with them is the
    /// 401 loop. The store belongs to the daemon, so the wipe is a request
    /// rather than something this side does: it is the process holding the
    /// SQLite file open, and it stops itself once the file is gone.
    pub fn reset_and_pair_again(&mut self, cx: &mut Context<Self>) {
        self.app_state = AppState::Loading;

        let asked = self
            .client
            .take()
            .inspect(Session::forget_session)
            .is_some();
        self.event_task.take();

        // Everything hydrated from the old device is now stale.
        self.chats.clear();
        self.selected_chat = None;
        self.message_list_cache.borrow_mut().clear();

        if !asked {
            self.app_state =
                AppState::Error("Not connected to the daemon, so nothing was cleared".to_string());
            cx.notify();
            return;
        }

        // Reconnecting rides straight into the same retry loop a cold start
        // uses: the old daemon is still closing the database it has just been
        // told to delete, so the first attempts fail and the loop keeps
        // starting one until the lock it is waiting on is free.
        self.retry_connection(cx);
    }

    pub fn retry_connection(&mut self, cx: &mut Context<Self>) {
        self.app_state = AppState::Loading;

        // Drop the old connection first: a second one alongside it would be
        // served the whole history again for nothing.
        self.client.take();
        self.event_task.take();

        // Off the UI thread: connecting can mean starting a daemon and
        // waiting for it to listen, which is a spinner rather than a frozen
        // window only if it happens somewhere else. A failure routes back to
        // the error screen, where retry stays available.
        self.reconnect_task = Some(cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            let connected = cx.background_spawn(async { Session::connect() }).await;
            let _ = entity.update(cx, |app, cx| {
                match connected {
                    Ok((client, ui_rx)) => {
                        app.event_task = Some(Self::spawn_event_task(ui_rx, cx));
                        app.client = Some(client);
                    }
                    Err(e) => {
                        app.app_state = AppState::Error(format!("Failed to reach the daemon: {e}"));
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Initialize the isolated input area view (needs window context)
    /// The InputAreaView has its own render cycle, so typing doesn't trigger app re-renders.
    /// IMPORTANT: This method should NOT update the InputAreaView on every call,
    /// as that would defeat the purpose of isolation.
    pub fn ensure_input_area(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input_area.is_none() {
            // Create the isolated input area view
            let input_area = cx.new(|cx| InputAreaView::new(window, cx));

            // Subscribe to events from the input area
            cx.subscribe(&input_area, Self::handle_input_area_event)
                .detach();

            self.input_area = Some(input_area);
        }
        // NOTE: Do NOT call input_area.update() here - it would trigger re-renders
        // on every parent render, defeating the purpose of component isolation.
        // Recording state is updated via update_input_recording() when it changes.
    }

    /// Handle events from the isolated input area view
    fn handle_input_area_event(
        &mut self,
        _input_area: Entity<InputAreaView>,
        event: &InputAreaEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputAreaEvent::SendMessage(text) => {
                self.send_message(text, cx);
            }
            InputAreaEvent::StartRecording => {
                self.start_recording(cx);
            }
            InputAreaEvent::StopRecording => {
                self.stop_recording_and_send(cx);
            }
            InputAreaEvent::CancelRecording => {
                self.cancel_recording(cx);
            }
            InputAreaEvent::CancelReply => {
                self.cancel_reply(cx);
            }
            InputAreaEvent::StartedTyping => {
                // Send "composing" presence
                if let Some(jid) = &self.selected_chat {
                    self.composing_chat = Some(jid.clone());
                    if let Some(client) = &self.client {
                        client.send_composing(jid);
                    }
                }
            }
            InputAreaEvent::StoppedTyping => {
                // Send "paused" presence to the chat the composing went to,
                // not whatever chat is selected when the timeout fires
                let target = self
                    .composing_chat
                    .take()
                    .or_else(|| self.selected_chat.clone());
                if let Some(jid) = target
                    && let Some(client) = &self.client
                {
                    client.send_paused(&jid);
                }
            }
        }
    }

    /// Unique optimistic-bubble id.
    ///
    /// A millisecond timestamp alone collides on fast double-sends
    /// (`add_message` would dedup one bubble away and `MessageIdAssigned`
    /// could rename the wrong one), and a timestamp plus a counter collides
    /// across processes: two windows on the same daemon each start their
    /// counter at zero, and the daemon broadcasts every assignment to both.
    /// The process id is what keeps them apart, and it also namespaces the
    /// media-cache file a voice note is staged in.
    fn next_local_id(prefix: &str) -> String {
        use portable_atomic::AtomicU64;
        use std::sync::atomic::Ordering;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        format!(
            "{prefix}_{}_{}_{}",
            std::process::id(),
            whatsapp_rust::wacore::time::now_millis(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Send a message to the currently selected chat
    fn send_message(&mut self, text: &str, cx: &mut Context<Self>) {
        // Check if connected before attempting to send
        if !self.is_connected() {
            warn!("Cannot send message: not connected");
            return;
        }

        let Some(jid) = self.selected_chat.clone() else {
            return;
        };

        let local_id = Self::next_local_id("local");
        let Some(client) = &self.client else {
            warn!("Cannot send message: client is unavailable");
            return;
        };
        client.send_message(&jid, text, local_id.clone());

        // Add to local chat immediately for responsiveness; the client renames
        // it to the real id via MessageIdAssigned.
        let msg = ChatMessage::new_outgoing(local_id, text.to_string());
        if self.add_message_to_chat(&jid, msg) {
            self.scroll_to_last_message();
        }

        cx.notify();
    }

    // ========== PTT Recording ==========

    // ========== Call State ==========

    // ========== Media Playback Control ==========

    // ========== Audio Playback ==========

    // ========== Video Playback ==========

    /// Start the video frame update task
    fn start_video_update_task(&mut self, cx: &mut Context<Self>) {
        // Cancel any existing task
        self.video_update_task = None;

        // Get completion receiver from current video player
        // Clone the message_id first to avoid borrow conflicts
        let msg_id = self.playing_video_id().map(|s| s.to_string());
        let completion_rx = msg_id
            .as_ref()
            .and_then(|id| self.video_players.get_mut(id))
            .map(|player| player.on_complete());

        // Spawn update loop (~30 fps) with completion handling
        self.video_update_task = Some(cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            // Create a fused future for completion (handles None case)
            let mut completion_rx = completion_rx;

            loop {
                // Check for completion event (non-blocking)
                if let Some(ref mut rx) = completion_rx {
                    // Try to receive without blocking
                    match rx.try_recv() {
                        Ok(()) => {
                            // Video completed naturally
                            let _ = entity.update(cx, |app, cx| {
                                app.active_media = ActiveMedia::None;
                                app.video_update_task = None;
                                app.audio_player.stop();
                                // Ownership must not survive the sink: a stale
                                // owner reads to the resume path as proof the
                                // audio can be resumed, replaying the video
                                // silently.
                                app.audio_owner = None;
                                cx.notify();
                            });
                            break;
                        }
                        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                            // Channel closed (player dropped or stopped manually)
                            break;
                        }
                        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                            // Not completed yet, continue updating frames
                        }
                    }
                }

                // Wait for next frame (~30 fps)
                smol::Timer::after(std::time::Duration::from_millis(33)).await;

                // Update frame
                let should_stop = entity
                    .update(cx, |app, cx| {
                        // Clone message_id first to avoid borrow conflicts
                        let msg_id = app.playing_video_id().map(|s| s.to_string());
                        if let Some(ref id) = msg_id
                            && let Some(player) = app.video_players.get_mut(id)
                        {
                            if player.update() {
                                cx.notify();
                            }
                            // Continue as long as we're in Playing state
                            return player.state() != VideoPlayerState::Playing;
                        }
                        true // Stop if no playing video
                    })
                    .unwrap_or(true);

                if should_stop {
                    let _ = entity.update(cx, |app, cx| {
                        app.active_media = ActiveMedia::None;
                        app.video_update_task = None;
                        app.audio_player.stop();
                        app.audio_owner = None;
                        cx.notify();
                    });
                    break;
                }
            }
        }));
    }

    // ========== Event Handling ==========

    /// Handle a received message
    fn handle_message_received(
        &mut self,
        chat_jid: String,
        mut message: ChatMessage,
        sender_name: Option<String>,
    ) {
        // Parse JID to determine chat type
        let jid = chat_jid.parse::<Jid>().ok();
        let is_group = jid.as_ref().is_some_and(|j| j.is_group());
        let is_status = jid.as_ref().is_some_and(|j| j.is_status_broadcast());

        // A message landing in the currently open chat is read immediately:
        // receipt out now, no badge (select_chat won't re-run to send it).
        let read_now =
            !message.is_from_me && self.selected_chat.as_deref() == Some(chat_jid.as_str());

        // Cache the sender's name if provided
        if let Some(ref name) = sender_name {
            self.name_cache.insert(message.sender.clone(), name.clone());
        }

        // For group chats, set sender_name on the message for display
        if is_group && !message.is_from_me {
            message.sender_name = sender_name
                .clone()
                .or_else(|| self.name_cache.get(&message.sender).cloned());
        }

        // Find the chat index so we can move it to the top after adding message
        let chat_index = self.chats.iter().position(|c| c.jid == chat_jid);

        if let Some(index) = chat_index {
            // Update the existing chat
            let chat = &mut self.chats[index];

            // For groups: update participant name, NOT the chat name
            if is_group {
                if let Some(ref name) = sender_name {
                    chat.update_participant(message.sender.clone(), name.clone());
                }
            } else if !is_status {
                // For DMs only: update chat name if we have a better one
                if let Some(ref name) = sender_name
                    && !message.is_from_me
                {
                    chat.set_name_if_not_worse(name.clone(), 2);
                }
            }
            // Status broadcasts: don't update any names
            let advanced = chat.add_message(message);

            // Move chat to top of list (most recent first); duplicates and
            // older backfills don't reorder
            if advanced {
                self.move_chat_to_top(index);
            }

            // Always invalidate caches since chat content changed
            self.invalidate_chat_cache();
            self.invalidate_message_cache(&chat_jid);
        } else {
            // Create new chat
            let display_name = if is_group || is_status {
                // For groups/status, don't use sender name as chat name
                None
            } else if message.is_from_me {
                // For outgoing DMs, use cached name
                self.name_cache.get(&chat_jid).cloned()
            } else {
                // For incoming DMs, use sender name
                sender_name.clone()
            };

            let mut new_chat = if let Some(name) = display_name {
                Chat::with_name(chat_jid.clone(), name)
            } else {
                Chat::new(chat_jid.clone())
            };

            // For groups: track participant
            if is_group && let Some(ref name) = sender_name {
                new_chat.update_participant(message.sender.clone(), name.clone());
            }

            new_chat.add_message(message);
            self.chats.insert(0, new_chat);
            self.invalidate_chat_cache();
        }

        if read_now {
            let newest = self
                .find_chat(&chat_jid)
                .and_then(|chat| chat.messages.last().map(|m| m.id.clone()));
            if let Some(client) = &self.client {
                // Receipt and the persisted read in one: see `select_chat`.
                client.mark_chat_read(&chat_jid, newest);
            }
            if let Some(chat) = self.find_chat_mut(&chat_jid) {
                chat.mark_as_read();
            }
            self.invalidate_chat_cache();
            self.invalidate_message_cache(&chat_jid);
        }
    }

    /// Handle a receipt event (read/played status update)
    /// A receipt about our own messages: advance their ticks.
    ///
    /// Only ever moves forward. Receipts arrive out of order and another of
    /// the peer's devices can repeat a delivery ack after the read one, so a
    /// naive assignment makes bubbles flicker from ✓✓ back to ✓.
    fn handle_receipt_received(
        &mut self,
        chat_jid: String,
        message_ids: Vec<String>,
        receipt_type: ReceiptType,
    ) {
        let status = match receipt_type {
            ReceiptType::Delivered => MessageStatus::Delivered,
            // Played is Read plus "and listened to it"; the ticks are the same.
            ReceiptType::Read | ReceiptType::ReadSelf => MessageStatus::Read,
            ReceiptType::Played | ReceiptType::PlayedSelf => MessageStatus::Read,
            // Retries, errors and sender echoes say nothing about delivery.
            _ => return,
        };

        let Some(chat) = self.find_chat_mut(&chat_jid) else {
            return;
        };
        let advanced = chat.advance_status(&message_ids, status);
        // A read receipt is also the peer telling us they opened the chat,
        // which clears our own unread state for the messages named.
        let read = if status == MessageStatus::Read {
            chat.mark_messages_as_read(&message_ids)
        } else {
            0
        };

        if advanced + read > 0 {
            debug!(
                "{:?} for {} message(s) in {}",
                receipt_type,
                advanced + read,
                observe_str(&chat_jid)
            );
            // Ticks and the unread badge changed; count-based cache guards
            // can't see either.
            self.invalidate_message_cache(&chat_jid);
            self.invalidate_chat_cache();
        }
    }

    /// Someone started or stopped typing.
    fn handle_chat_presence(
        &mut self,
        chat_jid: String,
        sender_jid: String,
        sender_name: Option<String>,
        composing: Option<ComposingKind>,
        cx: &mut Context<Self>,
    ) {
        match composing {
            Some(kind) => {
                // The push name is the best label, but a group's participant
                // map usually knows the person better, and the JID is the
                // honest last resort.
                let name = sender_name
                    .or_else(|| {
                        self.find_chat(&chat_jid)
                            .and_then(|chat| chat.participants.get(&sender_jid).cloned())
                    })
                    .or_else(|| self.name_cache.get(&sender_jid).cloned())
                    .unwrap_or_else(|| sender_jid.clone());
                self.presence
                    .set_composing(chat_jid, sender_jid, name, kind);
                // Nothing is ticking unless something needs expiring, and a
                // `composing` with no matching `paused` is exactly that.
                self.ensure_tick(cx);
            }
            None => self.presence.clear_composing(&chat_jid, &sender_jid),
        }
        self.invalidate_chat_cache();
        cx.notify();
    }

    /// Who is typing in a conversation, for the header and the timeline.
    pub fn typing_in(&self, chat_jid: &str) -> Option<TypingSummary> {
        self.presence.typing(chat_jid)
    }

    /// Where a contact is, for the header subtitle.
    pub fn availability_of(&self, jid: &str) -> Option<&Availability> {
        self.presence.availability(jid)
    }

    /// Handle a reaction event
    fn handle_reaction_received(
        &mut self,
        chat_jid: String,
        message_id: String,
        sender: String,
        emoji: String,
    ) {
        if let Some(chat) = self.find_chat_mut(&chat_jid) {
            if chat.add_reaction(&message_id, emoji.clone(), sender.clone()) {
                // Invalidate cache since reactions affect message height
                self.invalidate_message_cache(&chat_jid);
                info!(
                    "Added reaction '{}' from {} to message {} in {}",
                    emoji,
                    observe_str(&sender),
                    message_id,
                    observe_str(&chat_jid)
                );
            } else {
                info!(
                    "Message {} not found for reaction in chat {}",
                    message_id,
                    observe_str(&chat_jid)
                );
            }
        }
    }
}

impl Focusable for WhatsAppApp {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.chat_list_focus.clone()
    }
}

impl Render for WhatsAppApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity().clone();

        // Window-level commands hang off the root so they work wherever focus
        // happens to be, which is the point of a window-level command.
        let root = div()
            .size_full()
            .on_action(cx.listener(|app, _: &FocusSearch, window, cx| {
                app.focus_search(window, cx);
            }))
            .on_action(cx.listener(|app, _: &OpenSettings, window, cx| {
                app.open_settings(window, cx);
            }))
            .on_action(cx.listener(|app, _: &CloseOverlay, window, cx| {
                app.close_overlay(window, cx);
            }))
            .on_action(cx.listener(|app, _: &ReturnToCall, _window, cx| {
                app.return_to_call(cx);
            }));

        let body = match &self.app_state {
            AppState::Loading => render_loading_view(cx).into_any_element(),
            AppState::Connecting => render_connecting_view(cx).into_any_element(),
            AppState::WaitingForPairing {
                qr_code,
                pair_code,
                timeout_secs,
            } => render_pairing_view(qr_code.as_ref(), pair_code.clone(), *timeout_secs, cx)
                .into_any_element(),
            AppState::Syncing => render_syncing_view(cx).into_any_element(),
            // Settings is a screen over the conversation view, so it takes
            // the whole frame while it is open rather than floating.
            AppState::Connected if self.settings.is_some() => {
                render_settings_view(self, window, cx).into_any_element()
            }
            AppState::Connected => render_connected_view(self, window, cx).into_any_element(),
            AppState::Error(msg) => render_error_view(
                msg,
                self.retry_countdown(),
                self.error_detail_open,
                entity,
                cx,
            )
            .into_any_element(),
            AppState::LoggedOut { message } => {
                render_logged_out_view(message, entity, cx).into_any_element()
            }
        };

        root.child(body)
    }
}
