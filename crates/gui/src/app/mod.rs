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
mod paging;
mod recording;
mod recovery;
mod search;
mod settings;
mod status;
mod timeline_ctl;
mod viewer;

pub use calls::CallCard;
pub use chat_row::{ChatRow, Preview, PreviewGlyph, Unread};
pub use chats::{ChatFilter, ChatListCache, Survival, survives_complete_load};
pub use media::RecordingState;
pub use messages::{MessageListCache, TimelineItem};
pub use paging::nearing_end;

/// What the conversation list was last told about.
/// What the audio sink is holding, and where it came from.
///
/// A voice note keeps its encoded bytes: re-timing for a speed change, and
/// replaying after the clip has run out, both prepare the samples again from
/// the source rather than asking for the download twice. A video's
/// soundtrack has no source to keep — it is fed from the decoder — which is
/// exactly why the two cannot be one `Option<String>` with a loose
/// `Option<bytes>` beside it.
#[derive(Default)]
enum AudioHolder {
    #[default]
    None,
    Note {
        message_id: String,
        source: Arc<Vec<u8>>,
    },
    VideoTrack {
        message_id: String,
    },
}

impl AudioHolder {
    /// Whose sound this is, whatever kind it is.
    fn message_id(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Note { message_id, .. } | Self::VideoTrack { message_id } => Some(message_id),
        }
    }

    /// The bytes to prepare again, if this is `message_id`'s voice note.
    ///
    /// `None` for a video's track on purpose: there is nothing to re-time,
    /// and asking would be asking to play a soundtrack as a voice note.
    fn note_source(&self, message_id: &str) -> Option<Arc<Vec<u8>>> {
        match self {
            Self::Note {
                message_id: id,
                source,
            } if id == message_id => Some(Arc::clone(source)),
            _ => None,
        }
    }
}

/// What a recording in progress will be sent as.
///
/// Bound when the microphone opens, not read when it closes: the user can
/// switch chats or pick a different message to answer while it runs, and both
/// would otherwise be resolved against whatever the window looks like at the
/// end. One value rather than two fields, because the destination and the
/// reply are one answer to "where is this note going" and drifting apart is
/// exactly how a note reaches chat A quoting chat B.
struct RecordingTarget {
    jid: String,
    reply: Option<ReplyDraft>,
}

/// Which surface the keyboard belongs to.
///
/// A transient surface that takes focus has to give it back, and to exactly
/// one place: a blurred handle leaves the window with no keyboard target at
/// all, which reads as a window that has stopped listening. Naming the owner
/// is what makes "give it back" a single rule rather than something every
/// teardown path has to remember.
#[derive(Clone, Debug, PartialEq, Eq)]
enum KeyboardOwner {
    /// Where typing goes, when there is somewhere to type.
    Composer,
    /// The conversation list, which owns the arrow keys that move between
    /// chats. The resting owner of a window with no conversation open.
    ChatList,
    /// The window itself: the one surface that is always on screen, and so
    /// the only honest answer when nothing more specific is drawn. Its
    /// actions are the window-level ones — search, settings, escape — which
    /// were unreachable for exactly as long as nobody had focus at all,
    /// because a focus handle that is not in the frame sends dispatch to
    /// gpui's own root and never reaches ours. On a machine with a pointer
    /// the first click hid it; on one without, the window never listened.
    Root,
    /// A call that is ringing — not one that has been answered, which is a
    /// call people type through.
    RingingCall(String),
    /// The fullscreen viewer, which owns the arrow keys while it is up.
    Viewer,
    /// A screen with its own controls — Settings. It is handed the window's
    /// own handle rather than any control of its own: the way *out* of
    /// Settings is Escape, which is a window-level action, and the first
    /// control the user clicks takes the keyboard from there without a fight,
    /// because the sync only acts when the owner changes. Naming it
    /// separately is what makes *leaving* Settings a change — otherwise the
    /// owner still read `Composer` throughout, the next sync saw nothing to
    /// do, and the window was left with its keyboard on a control that had
    /// stopped being rendered.
    Screen,
}

/// Which keyboard surfaces a frame drew.
///
/// Answered by the view that draws them and read by `sync_overlay_focus`,
/// which may only hand the keyboard to something in the frame.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyboardSurfaces {
    /// The conversation list.
    pub chat_list: bool,
    /// The composer — which the offline strip replaces, and which a phone
    /// showing its list is not drawing at all.
    pub composer: bool,
    /// The fullscreen viewer. Held open across a dropped connection —
    /// `leave_connected_view` does not close it — while the error screen that
    /// replaces the conversation draws nothing of it.
    pub viewer: bool,
    /// The call card, which only the connected screens float.
    pub call_card: bool,
}

/// The layout a set of row heights was measured against.
///
/// The list keeps one measured height per index and nothing about a row says
/// how wide or how large it was drawn — so a window that resizes leaves every
/// one of those heights describing a bubble that no longer exists at that
/// size. Both halves move on their own: the fit changes the rem at a step
/// boundary without the pane changing width, and dragging an edge changes the
/// width without crossing a step.
#[derive(Clone, Copy, PartialEq)]
struct MeasuredAgainst {
    /// The base font in force, which every dimension in a bubble resolves
    /// from.
    rem: f32,
    /// How wide the timeline had to lay text out in, which is what decides
    /// how many lines a message takes.
    width: f32,
}

struct TimelineAnchor {
    jid: String,
    /// The layout the rows were measured against.
    measured: MeasuredAgainst,
    /// The rows themselves, as the list measured them.
    ///
    /// Kept rather than described: what the list holds is a height per row,
    /// so the only honest question is which of *these* rows the next frame
    /// still draws, and where. Cheap to keep — the rows and the messages
    /// behind them are both `Arc`s — and cheap to ask, because the `build`
    /// number says when they cannot have changed at all.
    rows: MessageListCache,
}

/// What one frame's rows mean for the measurements the list is keeping.
///
/// The decision `sync_timeline` makes, apart from the list it makes it about,
/// so it can be reasoned about — and tested — without a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimelineSync {
    /// One stretch of rows replaced by another, and every measurement outside
    /// it still describes the row it measured.
    ///
    /// Every ordinary change is this: an arrival replaces nothing at the end,
    /// a page of older history replaces nothing *after the encryption notice*
    /// — which is why the front is not index 0 — and a revoke or a run break
    /// replaces a row with itself. One frame can carry several of those; the
    /// answer is the stretch that covers them all.
    Spliced {
        at: usize,
        removed: usize,
        added: usize,
    },
    /// The same rows, against a layout that has moved under them.
    Remeasure,
    /// A different conversation. Nothing measured describes anything drawn.
    Reset,
    /// Nothing changed that the list has to hear about.
    Nothing,
}

/// What to do with the measurements the list is keeping.
///
/// `previous` is the anchor from the last frame and `rows` the ones this
/// frame will draw. The two questions are whether the rows the list measured
/// are still those rows, and if not, whether what changed is something a
/// splice can express.
fn timeline_sync(
    previous: Option<&TimelineAnchor>,
    jid: &str,
    rows: &MessageListCache,
    measured: MeasuredAgainst,
) -> TimelineSync {
    let Some(anchor) = previous.filter(|anchor| anchor.jid == jid) else {
        return TimelineSync::Reset;
    };
    let moved = anchor.measured != measured;

    // The rows were never rebuilt, so they are the same rows: only the layout
    // can have moved under them. This is the common frame, and it is what
    // keeps the diff below off the hot path.
    if anchor.rows.build == rows.build {
        return if moved {
            TimelineSync::Remeasure
        } else {
            TimelineSync::Nothing
        };
    }

    // What the two have in common at either end, and therefore what changed
    // between them. Asked of the rows rather than of their number, because a
    // count cannot tell an arrival from a page from a removal — and because
    // neither end is where a naive answer would put it: the encryption notice
    // holds index 0 whatever arrives in front of the messages, and the typing
    // indicator holds the last index whatever arrives behind them.
    let at = anchor.rows.common_prefix(rows);
    let kept = anchor.rows.common_suffix(rows, at);
    let removed = anchor.rows.items.len() - at - kept;
    let added = rows.items.len() - at - kept;

    if removed + added > 0 {
        return TimelineSync::Spliced { at, removed, added };
    }
    // The same rows, rebuilt: an image arrived, a reaction landed, a message
    // was revoked or a send grew a retry button. Nothing moved and every
    // height is suspect, which is what a rebuild means — `Nothing` here left
    // the list drawing a bubble at the size it used to be.
    TimelineSync::Remeasure
}
pub use search::ConversationSearch;
pub use settings::{SettingsSection, SettingsState};
pub use status::{Destination, StatusPane};
pub use viewer::MediaViewer;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use indexmap::IndexMap;

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, Image, KeyBinding, ListState, ScrollStrategy,
    Task, WeakEntity, Window, actions, div, prelude::*,
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
        /// The previous picture in the fullscreen viewer.
        ViewerPrev,
        /// The next picture in the fullscreen viewer.
        ViewerNext,
    ]
);

/// Reply to one message, named by its id.
///
/// Carries its subject rather than acting on "the selected message": a
/// timeline has no selection, and a context menu is opened on a specific
/// bubble. `no_json` because these are never bound in a keymap file — there is
/// no message id to write into one.
#[derive(Clone, PartialEq, gpui::Action)]
#[action(namespace = message, no_json)]
pub struct ReplyToMessage {
    pub id: gpui::SharedString,
}

/// Put one message's text on the clipboard.
#[derive(Clone, PartialEq, gpui::Action)]
#[action(namespace = message, no_json)]
pub struct CopyMessage {
    pub text: gpui::SharedString,
}

/// Send a message that failed again.
#[derive(Clone, PartialEq, gpui::Action)]
#[action(namespace = message, no_json)]
pub struct RetryMessage {
    pub id: gpui::SharedString,
}

use crate::components::{
    AccountSummary, InputAreaEvent, InputAreaView, ReplyDraft, new_timeline_state,
};
use log::{debug, error, info, warn};
use wacore_binary::jid::{Jid, JidExt, observe_str};

use crate::responsive::{MobilePanel, ResponsiveLayout};
use crate::session::{FromDaemon, Session};
use crate::theme::ActiveProductTheme as _;
use crate::utils::mime_to_image_format;
use crate::video::{StreamingVideoDecoder, VideoPlayer, VideoPlayerState};
use crate::views::pairing::generate_qr_png;
use crate::views::{
    render_call_overlay, render_connected_view, render_connecting_view, render_error_view,
    render_loading_view, render_logged_out_view, render_pairing_view, render_settings_view,
    render_syncing_view,
};
use oxidezap_audio::{AudioPlayer, AudioRecorder, encode_to_opus_ogg, generate_waveform};
use oxidezap_core::{
    ActiveCall, AppState, Availability, CachedQrCode, CallOutcome, CallRecord, CallState, Chat,
    ChatMessage, ComposingKind, DownloadableMedia, Ending, IncomingCall, Issued, MediaContent,
    MediaType, MessageStatus, OutgoingCall, PresenceRegistry, QuotedMessage, ReceiptType, Resend,
    Stage, SystemNotice, TypingSummary, UiEvent,
};

// ChatListCache is now in chats.rs and re-exported above
// RecordingState is now in media/mod.rs and re-exported above
// MessageListCache is now in messages.rs and re-exported above

/// Key context for chat list keyboard navigation
const CHAT_LIST_CONTEXT: &str = "ChatList";

/// Key context for the call card. Scoped rather than global so Enter/Escape
/// keep their meaning in the composer while no call is up.
pub const CALL_CONTEXT: &str = "Call";

/// Key context for the fullscreen viewer, so the arrow keys walk pictures
/// there and still move the caret in the composer everywhere else.
pub const VIEWER_CONTEXT: &str = "MediaViewer";

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
    let download = async {
        download_rx
            .await
            .unwrap_or(Err("Download cancelled".to_string()))
    };

    // Race between download and timeout
    crate::platform::with_timeout(
        download,
        std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS),
    )
    .await
    .ok_or_else(|| "Download timed out".to_string())?
}

/// Open the connection, on whichever thread can open one.
///
/// Off the UI thread on a desktop: connecting there can mean starting a
/// daemon and waiting for it to listen, which is a spinner rather than a
/// frozen window only if it happens somewhere else.
///
/// On the *window's own* thread in a browser, and that is not a preference.
/// gpui's background executor is a real worker there, and a worker has no
/// `window` — so the socket's URL would silently ignore the page's
/// `?daemon=`, and every media fetch afterwards would fail for want of
/// something to fetch from. There is nothing to move off the thread anyway:
/// a page cannot start a daemon, and its socket opens asynchronously, so
/// this returns immediately.
async fn attach(cx: &mut gpui::AsyncApp) -> std::io::Result<(Session, crate::session::Events)> {
    #[cfg(not(target_family = "wasm"))]
    {
        cx.background_spawn(async { Session::connect() }).await
    }
    #[cfg(target_family = "wasm")]
    {
        let _ = cx;
        Session::connect()
    }
}

/// What distinguishes this front end from another one on the same daemon.
///
/// A process id on the desktop, where two windows are two processes. Two tabs
/// are *one* process — and on the web, one that reports the same id in every
/// tab — so a random number stands in there: without it two tabs starting
/// their counters at zero would mint the same optimistic id within a
/// millisecond of each other, and the daemon broadcasts every assignment to
/// both, so one tab's send would rename or dedup the other's bubble.
///
/// Drawn once and kept, because it names the tab rather than the message.
fn front_end_id() -> u64 {
    #[cfg(not(target_family = "wasm"))]
    {
        u64::from(std::process::id())
    }
    #[cfg(target_family = "wasm")]
    {
        use portable_atomic::AtomicU64;
        use std::sync::atomic::Ordering;

        static TAB: AtomicU64 = AtomicU64::new(0);
        let known = TAB.load(Ordering::Relaxed);
        if known != 0 {
            return known;
        }
        // The browser's own generator: seeded properly, and already reached
        // for by everything under `wacore` that needs randomness.
        let mut bytes = [0u8; 8];
        // A tab that cannot be told apart from another is worse than one
        // whose number is a clock reading, so a refused draw still produces
        // something rather than zero.
        let drawn = match getrandom::fill(&mut bytes) {
            Ok(()) => u64::from_le_bytes(bytes),
            Err(e) => {
                log::warn!("no randomness for this tab's id ({e}); using the clock");
                wacore::time::now_millis().cast_unsigned()
            }
        };
        // Never zero, which is the "not drawn yet" marker.
        let drawn = drawn | 1;
        TAB.store(drawn, Ordering::Relaxed);
        drawn
    }
}

/// A name for media the sender never named.
///
/// The message id keeps two photos from the same conversation out of each
/// other's way, and the extension is what lets the file open at all.
fn default_media_name(message_id: &str, mime_type: &str) -> String {
    let extension = match mime_type.split(';').next().unwrap_or("").trim() {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        _ => "bin",
    };
    // Ids are opaque and long; the tail is enough to tell two apart and short
    // enough to stay a file name.
    let suffix: String = message_id
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .iter()
        .rev()
        .collect();
    format!("oxidezap-{suffix}.{extension}")
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
        // Window-wide, unlike the two above: only a ringing call takes the
        // keyboard (see `sync_call_focus`), and muting is something you do
        // *during* a call, with the caret back in the composer where someone
        // on a call is likely typing. A modifier chord has nothing to
        // collide with there, and it does nothing when no call is up.
        KeyBinding::new("secondary-shift-m", ToggleMute, None),
        KeyBinding::new("secondary-shift-c", ReturnToCall, None),
        // Walking pictures, scoped to the viewer for the same reason: the
        // arrow keys belong to the composer's caret everywhere else.
        KeyBinding::new("left", ViewerPrev, Some(VIEWER_CONTEXT)),
        KeyBinding::new("right", ViewerNext, Some(VIEWER_CONTEXT)),
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
    /// Focus target for the window itself, so the actions hung off the root
    /// are reachable whatever else is on screen — including on the screens on
    /// the way to a conversation, which have no list and no composer to
    /// focus.
    root_focus: FocusHandle,
    /// Which surface the keyboard was handed to, so it is handed over once
    /// and given back once. `None` until the first frame hands it somewhere:
    /// a window that starts out claiming an owner it never focused is a
    /// window the first sync finds nothing to do in, which is how the
    /// keyboard ended up belonging to nobody until the first click.
    /// See `sync_overlay_focus`.
    keyboard_owner: Option<KeyboardOwner>,
    /// Whether the last gesture that touched a conversation was someone
    /// meaning to *talk* to it or meaning to *look* at it.
    ///
    /// The distinction is [`ChatOpen`]'s and it already decided where focus
    /// went at the moment of the gesture; this is the same answer kept, so
    /// the frame after it does not undo it. Without it the composer outranked
    /// the list whenever both were drawn, and the first arrow key selected a
    /// chat and took the arrow keys away — the list's own bindings are scoped
    /// to it, so walking a list with the keyboard ended after one step.
    keyboard_intent: ChatOpen,
    /// Which of the two keyboard surfaces the last frame actually drew.
    ///
    /// Focus has to land on something that is *in* the frame — an unrendered
    /// handle sends every key to gpui's root instead — and only the view that
    /// draws them knows: the composer exists as an entity long after the
    /// conversation holding it left the screen, and on a phone the list and
    /// the conversation are the same slot.
    keyboard_surfaces: KeyboardSurfaces,
    /// Which playback the completion still in flight belongs to. See
    /// `stop_current_media`.
    playback_epoch: usize,
    /// The deadline `status_tick` is waiting on, so an earlier one that
    /// arrives later can replace it rather than queue behind it.
    status_tick_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The conversation the last frame actually drew, if any.
    ///
    /// Not the same as [`Self::selected_chat`], which is *kept* while the
    /// user reads statuses or, on a phone, walks the chat list — so that
    /// coming back lands where they were. Treating the selection as
    /// visibility is what made messages arriving in a hidden conversation
    /// read themselves, receipt and all, for someone who never saw them.
    /// Written by the render pass, because being on screen is a fact about
    /// what was drawn.
    visible_chat: Option<String>,
    /// Store-backed chats a complete load said were gone, kept on screen only
    /// because they are the open conversation.
    ///
    /// A complete load is the store's whole truth, so a store-backed chat
    /// missing from one was archived or deleted — possibly on another device.
    /// The selected chat is spared so the conversation being read is not
    /// yanked out from under it, and this is the other half of sparing it:
    /// without remembering the omission, the chat outlived its deletion until
    /// some unrelated later reload happened to notice again.
    departed_chats: std::collections::HashSet<String>,
    /// Chats opened before their messages arrived, whose reads are still owed.
    ///
    /// `MarkRead` is a claim about the message the requester was looking at,
    /// and the daemon refuses one that names nothing while it knows a
    /// boundary itself — rightly, since a read clears whole seconds. A row
    /// painted from the snapshot has no messages to name, so opening one
    /// straight from the first frame would clear the badge here, send no
    /// receipt, persist nothing, and have the badge come back on the next
    /// hydration. Held until the load that carries the messages, and spent
    /// there. A set rather than one chat: on the frame the snapshot paints,
    /// every row is a row without messages, so two of them can be opened
    /// before either load lands.
    owed_reads: std::collections::HashSet<String>,
    /// Where each conversation's timeline continues, and whether it is
    /// asking. See [`paging`].
    timeline_pages: paging::TimelinePages,
    /// Where the chat list continues.
    chat_pages: paging::Paging,
    /// Status updates watched in this window. Local by design — there is no
    /// receipt to send — and therefore this window's job to remember across a
    /// hydration merge, which replaces those rows from the store.
    watched_status: std::collections::HashSet<String>,
    /// Focus target for the fullscreen viewer, which owns the arrow keys
    /// only while it is up.
    viewer_focus: FocusHandle,
    /// Search input state for chat list (created lazily when window is available)
    chat_search_input: Option<Entity<InputState>>,
    /// The picture being looked at full screen, when one is.
    media_viewer: Option<MediaViewer>,
    /// Searching inside the open conversation, when that is open. Separate
    /// from the list's field, which filters chats by name.
    conversation_search: Option<ConversationSearch>,
    /// The field for it, created the first time the search is opened.
    conversation_search_input: Option<Entity<InputState>>,
    /// Current search query (lowercase, trimmed)
    chat_search_query: String,
    /// Debounced search task
    #[allow(dead_code)]
    chat_search_task: Option<Task<()>>,
    /// Scroll handle for message list
    /// The conversation's list, which measures its own rows.
    ///
    /// One state rather than one per chat: it holds measurements and a scroll
    /// position, both of which belong to what is on screen. Switching chats
    /// resets it, which is also what puts the reader at the newest message.
    message_list: ListState,
    /// What `message_list` was last reset or spliced for, so a new message
    /// extends it and a new conversation replaces it.
    timeline_anchor: Option<TimelineAnchor>,
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
    recording_target: Option<RecordingTarget>,
    /// Which account the answers still in flight belong to.
    ///
    /// A measurement asked of one daemon can land after the window has been
    /// handed to another, and the surfaces that show it — Settings, most of
    /// all — survive the change. Bumped by `forget_account_state`; an answer
    /// whose epoch no longer matches is dropped rather than displayed.
    account_epoch: usize,
    /// Which recording the encode still in flight belongs to.
    ///
    /// Encoding runs detached on the background pool and nothing can stop it,
    /// so cancelling is not a matter of aborting the work but of disowning
    /// its result. Bumped by `cancel_recording`; a completion whose epoch no
    /// longer matches is dropped rather than sent.
    recording_epoch: usize,
    /// Audio player for voice message and video audio playback
    audio_player: AudioPlayer,
    /// Playback speed for voice notes, shared across clips: someone who
    /// listens at 1.5× means it for the next note too.
    playback_speed: f32,
    /// Repaints the playhead while audio plays. Only alive while it does.
    #[allow(dead_code)]
    playback_tick: Option<Task<()>>,
    /// What the one audio sink is holding.
    ///
    /// One field, because the name and the bytes are one fact. As two, a stop
    /// cleared the name and left the bytes behind, and the next thing to read
    /// the pair — a speed change, a scrub past the end — took a video's
    /// message id and a voice note's samples and played the wrong sound
    /// against the wrong row.
    audio: AudioHolder,
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
    /// What call is happening. Adopted whole from the daemon on attach, and
    /// advanced by the same events the daemon applies to its own copy.
    call_state: CallState,
    /// Where *this* window puts the card for it.
    call_card: CallCard,
    /// Cache of JID -> display name mappings (from notify/pushname attribute)
    name_cache: HashMap<String, String>,
    /// System notices whose conversation has not arrived yet.
    ///
    /// "You were added to a group" reaches this window before the group does,
    /// every time. The store does not hold system notices, so the reload that
    /// finally brings the chat cannot bring the row with it — dropped here,
    /// it is gone for good.
    pending_notices: HashMap<String, Vec<(String, chrono::DateTime<chrono::Utc>, SystemNotice)>>,
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
    /// The account's own JID, for the number under the name.
    account_jid: Option<String>,
    /// The same account's LID. A chat with your own number can be keyed by
    /// either alias, and neither string matches the other.
    account_lid: Option<String>,
    /// The Settings screen, when it is open. `None` is the conversation view.
    settings: Option<SettingsState>,
    /// What this account occupies on disk, as the daemon last measured it.
    storage_usage: Option<crate::session::StorageUsage>,
    /// Which of the sidebar's destinations is on screen.
    destination: Destination,
    /// Whose status updates are open, and which one of them.
    status_pane: StatusPane,
    /// The broadcast grouped by author, against the message count it was
    /// built from.
    status_feed_cache: RefCell<Option<(usize, oxidezap_core::StatusFeed)>>,
    /// Repaints the call duration, and expires stale typing notices. Only
    /// alive while there is something to tick.
    #[allow(dead_code)]
    tick_task: Option<Task<()>>,
    /// Repaints the recording panel's clock and level meter. Only alive while
    /// the microphone is.
    #[allow(dead_code)]
    recording_tick: Option<Task<()>>,
    /// Redraws the status feed when its next update lapses. A status is the
    /// one thing that changes with nothing happening.
    #[allow(dead_code)]
    status_tick: Option<Task<()>>,
    /// Polls `theme.json` for an edit, and repaints the pairing countdown.
    /// Both are things that change with no event to carry them.
    #[allow(dead_code)]
    heartbeat: Option<Task<()>>,
}

impl WhatsAppApp {
    /// Spawn the event handling task that processes UI events from the WhatsApp client
    fn spawn_event_task(mut ui_rx: crate::session::Events, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            while let Some(message) = ui_rx.recv().await {
                let result = match message {
                    FromDaemon::Session(event) => entity.update(cx, |app, cx| {
                        app.handle_event(*event, cx);
                    }),
                    // Adopted whole rather than replayed: these are the calls
                    // that were already happening when this window attached,
                    // and a call this account placed was never an event.
                    FromDaemon::Calls(calls) => entity.update(cx, |app, cx| {
                        app.adopt_calls(*calls, cx);
                    }),
                    // Announced on connect, before this window existed, so it
                    // arrives with the snapshot rather than as an event.
                    FromDaemon::Account(account) => entity.update(cx, |app, cx| {
                        // `None` is an answer, not a missing one: a daemon
                        // with no account paired is saying so. Keeping the
                        // last identity meant the old account survived a
                        // re-pair — its name under the sidebar, its number
                        // beneath, and its JID still reading as "(You)" —
                        // until some later `AccountUpdated` corrected it.
                        app.account_name = account.as_ref().and_then(|a| a.name.clone());
                        app.account_jid = account.as_ref().and_then(|a| a.jid.clone());
                        app.account_lid = account.and_then(|a| a.lid);
                        cx.notify();
                    }),
                    // Refused, or it never left this process. The ring came
                    // down when the update was opened, which is right — a
                    // view that waits for a round trip flickers — so the
                    // correction is to put it back rather than to leave a
                    // watched update that returns new on the next start.
                    // One page of a conversation, for the timeline that
                    // asked. Folded in rather than replacing: the rows a
                    // page brings sit before the ones the window already has.
                    FromDaemon::Messages {
                        jid,
                        messages,
                        next,
                    } => entity.update(cx, |app, cx| {
                        app.apply_message_page(jid, messages, next, cx);
                    }),
                    FromDaemon::Chats { chats, next } => entity.update(cx, |app, cx| {
                        app.apply_chat_page(chats, next, cx);
                    }),
                    FromDaemon::PageLost { jid } => entity.update(cx, |app, _cx| {
                        app.page_lost(jid);
                    }),
                    FromDaemon::StatusViewLost(message_ids) => entity.update(cx, |app, cx| {
                        app.forget_status_views(&message_ids, cx);
                    }),
                    // The tray's "Open", or another front end asking on a
                    // user's behalf. One window, so there is one to raise.
                    FromDaemon::ShowWindow => {
                        cx.update(|cx| {
                            if let Some(window) = cx.windows().first() {
                                let _ = window.update(cx, |_, window, _| window.activate_window());
                            }
                        });
                        Ok(())
                    }
                };
                if result.is_err() {
                    // Entity was dropped, stop the loop
                    break;
                }
            }
        })
    }

    /// Create a new WhatsApp application
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            app_state: AppState::Loading,
            chats: Vec::new(),
            selected_chat: None,
            client: None,
            chat_list_scroll: VirtualListScrollHandle::new(),
            chat_list_focus: cx.focus_handle(),
            call_focus: cx.focus_handle(),
            root_focus: cx.focus_handle(),
            keyboard_owner: None,
            // Nothing has been opened to talk to yet, and a window that comes
            // up on a restored selection is one nobody has typed into.
            keyboard_intent: ChatOpen::ToPreview,
            keyboard_surfaces: KeyboardSurfaces::default(),
            playback_epoch: 0,
            status_tick_at: None,
            visible_chat: None,
            departed_chats: std::collections::HashSet::new(),
            owed_reads: std::collections::HashSet::new(),
            timeline_pages: paging::TimelinePages::new(),
            chat_pages: paging::Paging::default(),
            watched_status: std::collections::HashSet::new(),
            viewer_focus: cx.focus_handle(),
            chat_search_input: None, // Created lazily when window is available
            media_viewer: None,
            conversation_search: None,
            conversation_search_input: None,
            chat_search_query: String::new(),
            chat_search_task: None,
            message_list: new_timeline_state(0),
            timeline_anchor: None,
            input_area: None,
            composing_chat: None,
            drafts: HashMap::new(),
            event_task: None,
            reconnect_task: None,
            audio_recorder: AudioRecorder::new(),
            recording_state: RecordingState::default(),
            recording_target: None,
            account_epoch: 0,
            recording_epoch: 0,
            audio_player: AudioPlayer::new(),
            playback_speed: 1.0,
            playback_tick: None,
            audio: AudioHolder::None,
            active_media: ActiveMedia::None,
            pending_media_request: None,
            retry_at: None,
            retry_task: None,
            error_detail_open: false,
            downloads_in_flight: std::collections::HashSet::new(),
            call_state: CallState::new(),
            call_card: CallCard::default(),
            name_cache: HashMap::new(),
            pending_notices: HashMap::new(),
            video_players: HashMap::new(),
            video_update_task: None,
            decoded_images: RefCell::new(IndexMap::new()),
            message_list_cache: RefCell::new(HashMap::new()),
            chat_list_cache: RefCell::new(None),
            storage_usage: None,
            destination: Destination::default(),
            status_pane: StatusPane::default(),
            status_feed_cache: RefCell::new(None),
            mobile_panel: MobilePanel::default(),
            chat_filter: ChatFilter::default(),
            reply_to: None,
            presence: PresenceRegistry::new(),
            account_name: None,
            account_jid: None,
            account_lid: None,
            settings: None,
            tick_task: None,
            status_tick: None,
            heartbeat: None,
            recording_tick: None,
        }
    }

    /// Reach the daemon, from a window that already exists.
    ///
    /// Separate from [`Self::new`] because connecting is not instant on a
    /// first launch: there is no daemon listening, so `connect_or_start`
    /// starts one and polls for it, which is up to ten seconds. Done in the
    /// constructor that was ten seconds with no window on screen at all —
    /// the loading state cannot be drawn by the entity that is still being
    /// built. Off the UI thread, exactly like the retry it shares.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        self.retry_connection(cx);
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
            self.visible_panel(),
            cx.product().metrics,
        )
    }

    /// Which panel a phone-width window is showing.
    ///
    /// Derived, not merely stored. A phone shows one panel at a time, and the
    /// stored one is a record of where the *user* went — which can name the
    /// conversation panel while there is no conversation in it: select a chat
    /// on a wide window, close it, then narrow the window, and the phone
    /// layout had a panel with nothing in it and no list to pick from. The
    /// list is what a window falls back to, because it is the one panel that
    /// is never empty of things to do.
    fn visible_panel(&self) -> MobilePanel {
        match self.mobile_panel {
            MobilePanel::Chat if !self.has_something_to_show() => MobilePanel::ChatList,
            panel => panel,
        }
    }

    /// Whether the conversation panel has anything in it, whichever
    /// destination the window is on.
    fn has_something_to_show(&self) -> bool {
        match self.destination {
            Destination::Chats => self.selected_chat.is_some(),
            Destination::Status => self.status_pane.is_open(),
        }
    }

    /// Get the current mobile panel state
    pub fn mobile_panel(&self) -> MobilePanel {
        self.mobile_panel
    }

    /// Navigate back to chat list (for mobile)
    pub fn navigate_back(&mut self, cx: &mut Context<Self>) {
        self.mobile_panel = MobilePanel::ChatList;
        // Leaving the panel means leaving what was in it: a status left open
        // would put the window straight back into it on the next layout,
        // since the panel is derived from whether there is anything to show.
        // Stopped before it is closed, and in that order — afterwards there
        // is no way to ask which update was on screen, so a video went on
        // decoding and playing behind the chat list.
        self.leave_shown_status();
        self.status_pane.close();
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
            .conversations()
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
                    self.is_own_number(&chat.jid),
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
        self.conversations()
            .filter(|chat| ChatFilter::Unread.matches(chat))
            .count()
    }

    /// The chats that are conversations.
    ///
    /// Every list of people to talk to goes through here, so the status
    /// broadcast is excluded once rather than at each of them: it is not a
    /// conversation, and it has [its own destination](Destination::Status).
    fn conversations(&self) -> impl Iterator<Item = &Chat> {
        self.chats.iter().filter(|chat| !chat.is_status)
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
    ///
    /// Present whenever this device is linked, whether or not the account has
    /// a push name: gating the whole row on the name is what made a live
    /// session read as "not linked".
    pub fn account_summary(&self) -> Option<AccountSummary> {
        if !self.is_linked() {
            return None;
        }
        let connected = matches!(self.app_state, AppState::Connected);
        Some(AccountSummary {
            name: self
                .account_name
                .clone()
                .or_else(|| self.account_jid.clone())
                .unwrap_or_else(|| "This device".to_string()),
            // Offline is a state the user chose, so it is named rather than
            // described as a connection still being attempted.
            status: match self.app_state {
                AppState::Connected => "linked device · synced".to_string(),
                AppState::Offline => "linked device · offline".to_string(),
                _ => "linked device · reconnecting".to_string(),
            },
            is_healthy: connected,
        })
    }

    /// The account's own number, when the session has said.
    pub fn account_jid(&self) -> Option<&str> {
        self.account_jid.as_deref()
    }

    /// Whether `jid` addresses this account's own number.
    ///
    /// Through the library's own comparison rather than a string match: the
    /// account is announced as a JID with a device on it and the chat is keyed
    /// without one, so the two never match outright, and `is_same_user_as` is
    /// the rule that already knows which parts of a JID are its identity.
    pub fn is_own_number(&self, jid: &str) -> bool {
        let Ok(other) = jid.parse::<Jid>() else {
            return false;
        };
        // Against both aliases, and on `user_base`: the account is announced
        // as a device JID whose user part carries the device after a colon
        // (`5599…:57`), and `is_same_user_as` compares that field raw — so the
        // one comparison that had to succeed never did. The LID is here
        // because the chat with your own number can be keyed by it while the
        // account announces a phone number, and neither string matches the
        // other.
        [self.account_jid.as_deref(), self.account_lid.as_deref()]
            .into_iter()
            .flatten()
            .filter_map(|own| own.parse::<Jid>().ok())
            .any(|own| own.user_base() == other.user_base())
    }

    /// Whether this device is paired at all.
    ///
    /// The states that mean "there is no account yet" are the ones that show
    /// a QR code or an ended session; everything else is a linked device,
    /// including one that is reconnecting or still syncing.
    fn is_linked(&self) -> bool {
        !matches!(
            self.app_state,
            AppState::WaitingForPairing { .. } | AppState::LoggedOut { .. }
        )
    }

    /// Invalidate chat list cache (call when chats change or search changes)
    ///
    /// The status feed is derived from the same chats, so it goes with it: a
    /// second cache that outlived the first would draw a run of updates that
    /// no longer matches the messages behind it.
    fn invalidate_chat_cache(&self) {
        *self.chat_list_cache.borrow_mut() = None;
        *self.status_feed_cache.borrow_mut() = None;
    }

    // ========== Message List Cache ==========

    /// The timeline's rows for a chat, rebuilt only when they changed.
    ///
    /// Also keeps `message_list` pointing at them: the list holds one row
    /// count and one set of measurements, so it has to learn about a row
    /// appearing at the same moment the rows do. Appending splices — which
    /// keeps the reader where they were — and anything else resets, which
    /// lands them at the newest message.
    pub fn get_message_list_cache(
        &mut self,
        chat_jid: &str,
        messages: &[ChatMessage],
        is_group: bool,
        typing: Option<TypingSummary>,
        layout: ResponsiveLayout,
    ) -> MessageListCache {
        let cached = {
            let cache = self.message_list_cache.borrow();
            cache
                .get(chat_jid)
                .filter(|cached| cached.is_valid_for(messages.len(), is_group, typing.as_ref()))
                .cloned()
        };
        let rows = cached.unwrap_or_else(|| {
            let built = MessageListCache::new(messages, is_group, typing);
            self.message_list_cache
                .borrow_mut()
                .insert(chat_jid.to_string(), built.clone());
            built
        });

        self.sync_timeline(
            chat_jid,
            &rows,
            MeasuredAgainst {
                rem: layout.metrics().rem_size(),
                width: layout.message_list_width(),
            },
        );
        // Asked from here rather than from a row's own render, because this is
        // the frame's one pass over the timeline and it already holds the
        // list. A conversation shorter than its viewport reports the top row
        // as visible and so asks straight away, which is right: there is more
        // behind it and nowhere to scroll to say so.
        if paging::nearing_start(self.message_list.logical_scroll_top().item_ix) {
            self.want_older_messages(chat_jid);
        }
        rows
    }

    /// Tell the list what its rows are now, in the way that preserves the
    /// most.
    ///
    /// The list keeps one measured height per row index, so what it is told
    /// has to match what actually changed. A count on its own cannot say: a
    /// history backfill inserts older messages *before* the head and raises
    /// the count doing it, which read as an append and spliced the new rows
    /// in at the wrong end; and a row that changes height without the count
    /// moving — an image arriving, a reaction, a revoke, a failed send
    /// growing its retry button — read as nothing having happened at all,
    /// leaving bubbles measured at a size they no longer are.
    ///
    /// So the question is asked of the rows: are the ones the list measured
    /// still those rows? The last of them is what answers it. An append
    /// leaves that row where it was; a prepend, a removal, and a row landing
    /// in the *middle* — a system notice stamped in the past, a backfilled
    /// message newer than the head but older than the tail — all push it
    /// along, and all of those raise the count exactly as an arrival does.
    /// Comparing the first row instead saw none of the middle three.
    fn sync_timeline(
        &mut self,
        chat_jid: &str,
        rows: &MessageListCache,
        measured: MeasuredAgainst,
    ) {
        let count = rows.items.len();
        let previous = self.timeline_anchor.take();

        match timeline_sync(previous.as_ref(), chat_jid, rows, measured) {
            // One stretch of rows for another, leaving every measurement
            // outside it in place — and the reader with it, since a
            // bottom-anchored list is moved by neither a page arriving above
            // nor a message arriving below. Reset instead, and a reader who
            // scrolled back far enough to ask for more was thrown to the
            // newest message for asking.
            TimelineSync::Spliced { at, removed, added } => {
                self.message_list.splice(at..at + removed, added);
                // Only while the measurements outside it still describe those
                // rows: a window resized in the same frame is one where they
                // do not, and a splice keeps every one of them.
                if previous.as_ref().is_some_and(|a| a.measured != measured) {
                    self.message_list.remeasure();
                }
            }
            // Something is a different size now; remeasure rather than reset,
            // which keeps the reader where they were.
            TimelineSync::Remeasure => self.message_list.remeasure(),
            TimelineSync::Reset => self.message_list.reset(count),
            TimelineSync::Nothing => {}
        }

        self.timeline_anchor = Some(TimelineAnchor {
            jid: chat_jid.to_string(),
            measured,
            rows: rows.clone(),
        });
    }

    /// Invalidate message list cache for a chat (call when messages change)
    /// Drop a chat's rendered timeline, and anything derived from it.
    ///
    /// `&mut self` because an open conversation search is derived from those
    /// same messages: every caller here is announcing that the chat's history
    /// changed, which is exactly when the search's matches stop describing it.
    fn invalidate_message_cache(&mut self, chat_jid: &str) {
        self.message_list_cache.borrow_mut().remove(chat_jid);
        self.refresh_conversation_search(chat_jid);
        self.refresh_media_viewer(chat_jid);
        // The status feed is a second view of one chat's messages, and its
        // own guard — the message count — cannot see a message *changing*.
        if Self::is_status_jid(chat_jid) {
            *self.status_feed_cache.borrow_mut() = None;
        }
    }

    /// Record what this frame draws as the conversation. See
    /// [`Self::visible_chat`].
    ///
    /// Also where a conversation asks for its history: a chat is *on screen*
    /// exactly when its timeline needs filling, and the frame that draws it
    /// is the one place that knows which chat that is — the selection can
    /// name a chat the window is not showing (Settings is up, the reader is
    /// in Status).
    pub fn note_visible_conversation(&mut self, jid: Option<String>) {
        if let Some(jid) = &jid {
            self.ensure_timeline_page(jid);
        }
        self.visible_chat = jid;
    }

    /// Whether the chat list this frame draws has no rows in it at all.
    ///
    /// Asked by the frame rather than by the list component: the virtual list
    /// that pages the sidebar is not built when there is nothing to put in
    /// it, so the one list that most needs another page — a filter matching
    /// nothing yet — would be the one list that never asks for one.
    pub fn chat_list_is_empty(&self) -> bool {
        self.get_chat_list_cache().rows.is_empty()
    }

    /// Record which keyboard surfaces this frame drew. See
    /// [`KeyboardSurfaces`].
    pub fn note_keyboard_surfaces(&mut self, surfaces: KeyboardSurfaces) {
        self.keyboard_surfaces = surfaces;
    }

    /// The window's own focus target, which every frame draws.
    pub fn root_focus(&self) -> &FocusHandle {
        &self.root_focus
    }

    /// Drop everything this window learned from the account it is leaving.
    ///
    /// Not only the chats. Anything keyed by a JID or a message id belongs to
    /// the account that produced it, and the next account can share those
    /// keys: a group survives a re-pair, and a contact certainly does. A
    /// draft is the one that bites — text composed under the old account
    /// reappearing in the composer of the new one, ready to be sent to
    /// someone it was never written for.
    fn forget_account_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The microphone and the speaker belong to the account too. This is
        // the same departure a disconnect or a logout makes — the connected
        // view is going away — and skipping it left capture running with no
        // controls to stop it, and an encode that could still finish, pass an
        // epoch nothing had bumped, and send the old account's note from the
        // newly paired one.
        self.leave_connected_view(cx);
        // A call is account state as much as a chat is. Left standing, the
        // next daemon's first (empty) snapshot reads as this stage ending, and
        // the record is written into the account that has just been paired —
        // recreating a chat for the old account's peer to hold it.
        self.call_state = CallState::new();
        self.call_card.call_ended();
        // What the *old* account occupied, and the query that is still
        // measuring it. Settings survives the reset, so a completion landing
        // after it would show the previous account's database and media under
        // the new one; `account_epoch` is what the detached task checks.
        self.storage_usage = None;
        self.account_epoch = self.account_epoch.wrapping_add(1);
        self.chats.clear();
        self.selected_chat = None;
        self.visible_chat = None;
        self.departed_chats.clear();
        // The cursors describe positions in one account's store; the next
        // account's rows are not behind them.
        self.forget_paging();
        self.owed_reads.clear();
        // The reader is a selection too, and a JID-keyed one. Left alone, it
        // pointed the new account at the old account's contact: at their
        // updates if that contact exists there — watched by nobody in this
        // account — and otherwise at an empty reader which, on a phone, is
        // the whole screen with no way back out of it.
        self.status_pane.close();
        self.destination = Destination::default();
        self.message_list_cache.borrow_mut().clear();
        self.chat_list_cache.borrow_mut().take();
        *self.status_feed_cache.borrow_mut() = None;
        self.decoded_images.borrow_mut().clear();
        self.timeline_anchor = None;
        // Composed text, and the reply bar it may be answering.
        self.drafts.clear();
        self.reply_to = None;
        if let Some(input) = self.input_area.clone() {
            input.update(cx, |view, cx| {
                view.set_reply(None, cx);
                view.swap_text("", window, cx);
            });
        }
        // Names, watched updates, notices with nowhere to go yet. Presence
        // and the composing state left with `leave_connected_view`, above.
        self.name_cache.clear();
        self.watched_status.clear();
        self.pending_notices.clear();
        // Anything mid-flight against a message id that is about to be gone.
        // Playback itself is already stopped, above.
        self.video_players.clear();
        self.downloads_in_flight.clear();
        self.media_viewer = None;
        self.conversation_search = None;
        self.chat_search_query.clear();
    }

    /// Everything that has to stop when the connected view goes away.
    ///
    /// The controls that stop them are drawn by that view, so anything still
    /// running when it is replaced has no way to be stopped: a recording
    /// holds the microphone open, ticks every 100ms and grows a buffer, and
    /// a voice note plays on over a screen that is now an error message.
    /// Three transitions leave that view — a disconnect, an error and a
    /// logout — which is three chances to forget, so it is one method.
    ///
    /// Not [`AppState::Offline`]: that keeps the conversation on screen and
    /// only refuses to send.
    fn leave_connected_view(&mut self, cx: &mut Context<Self>) {
        if self.recording_state != RecordingState::Idle {
            self.cancel_recording(cx);
        }
        self.stop_current_media();
        // Who was around was true of the connection that has just ended. A
        // typing notice expires on its own, but `Availability::Online` has no
        // TTL — nothing but another presence event ever takes it down, and
        // that event is exactly what a dropped connection stops delivering.
        // The header went on saying a contact was online for as long as the
        // window stayed open.
        self.presence = PresenceRegistry::new();
        // The composer may have said this window was typing. `composing_chat`
        // names a chat in the account being left, and the view's own timeout
        // would send its `paused` down whatever session comes next — after
        // swallowing the first keystroke there as an already-live
        // composition.
        self.composing_chat = None;
        if let Some(input) = self.input_area.clone() {
            input.update(cx, |view, _| view.reset_typing());
        }
    }

    /// Put the caret where typing goes.
    ///
    /// Also the place focus comes back to when a transient surface gives it
    /// up: a blurred handle leaves the window with no keyboard target at
    /// all, which reads as a window that stopped listening.
    pub(super) fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Putting the caret in the composer *is* the statement that typing
        // goes here now, so the owner model records it here rather than at
        // each of the three callers — a reply begun, a chat opened to talk
        // to, the sync handing it back.
        self.keyboard_intent = ChatOpen::ToCompose;
        if let Some(input_area) = self.input_area.clone() {
            // Read the handle out before focusing: `focus` needs `&mut App`
            // and `read` holds `cx` borrowed for as long as its result lives.
            let handle = input_area.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
    }

    /// Re-resolve an open media viewer against the chat's current messages.
    ///
    /// The viewer names the picture it is showing and resolves it on every
    /// frame, so a revoke behind it left a modal that drew nothing and still
    /// swallowed the Escape meant to close it — a window that had stopped
    /// responding, as far as anyone looking at it could tell. Reconciled here
    /// because this is the announcement that a chat's history changed, which
    /// is the whole set of ways a picture can stop existing.
    fn refresh_media_viewer(&mut self, chat_jid: &str) {
        if self
            .media_viewer
            .as_ref()
            .is_none_or(|viewer| viewer.jid != chat_jid)
        {
            return;
        }
        let Some(mut viewer) = self.media_viewer.take() else {
            return;
        };
        if self.viewer_survives(&mut viewer) {
            self.media_viewer = Some(viewer);
        }
    }

    /// Point `viewer` at what its chat holds now, and say whether anything is
    /// left to look at.
    ///
    /// Takes the viewer rather than reading it out of `self`, so the chat can
    /// be borrowed where it lies: the alternative is cloning a conversation's
    /// whole message vector to hand it to a viewer that owns three strings.
    fn viewer_survives(&self, viewer: &mut MediaViewer) -> bool {
        match self.find_chat(&viewer.jid) {
            Some(chat) => viewer.refresh(&chat.messages),
            // The conversation itself is gone; so is everything it held.
            None => false,
        }
    }

    /// Re-run an open conversation search over the chat's current messages.
    ///
    /// The matches were rebuilt only when the *query* changed, so a message
    /// arriving, a history merge, an edit or a revoke left the count and the
    /// navigation describing a conversation that had moved on — with no way
    /// to correct it but to retype the query. Called from every path that
    /// invalidates a chat's timeline, which is the same set of changes.
    fn refresh_conversation_search(&mut self, chat_jid: &str) {
        if self
            .conversation_search
            .as_ref()
            .is_none_or(|search| search.jid != chat_jid || search.query.is_empty())
        {
            return;
        }
        // Lifted out for the same reason as in `set_conversation_search`:
        // reading the messages needs `self` while the search is being written.
        let Some(mut search) = self.conversation_search.take() else {
            return;
        };
        let query = search.query.clone();
        if let Some(chat) = self.find_chat(&search.jid) {
            search.refresh(&query, &chat.messages);
        }
        self.conversation_search = Some(search);
    }

    // ========== Accessors ==========

    /// Check if the client is connected
    fn is_connected(&self) -> bool {
        matches!(self.app_state, AppState::Connected)
    }

    /// Whether this window can send anything at all.
    ///
    /// The composer, the call buttons and the recorder all hang off this: in
    /// [`AppState::Offline`] the history is readable and nothing else, and a
    /// control that accepted input there would produce a message with nowhere
    /// to go.
    pub fn can_send(&self) -> bool {
        self.is_connected()
    }

    /// Whether the user chose to stop waiting and read what is here.
    pub fn is_offline(&self) -> bool {
        matches!(self.app_state, AppState::Offline)
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

    /// Make sure there is a conversation for `jid` to put a row in.
    ///
    /// A call is the one thing that can name a peer this window has never had
    /// a conversation with — a first-time caller, or a stranger — and
    /// [`Self::add_message_to_chat`] answers "no such chat" by dropping the
    /// row. That silently discarded every declined, missed and completed
    /// record for such a call, badge included, so once the card went away
    /// nothing said the phone had rung. The inbound-message path already
    /// creates a chat for a sender it has not seen; this is the same rule for
    /// the caller.
    ///
    /// Live-only, so a later complete store load leaves it alone rather than
    /// pruning it as a chat that was deleted elsewhere.
    fn ensure_chat(&mut self, jid: &str) {
        if self.chats.iter().any(|chat| chat.jid == jid) {
            return;
        }
        let chat = match self.name_cache.get(jid) {
            Some(name) => Chat::with_name(jid.to_string(), name.clone()),
            None => Chat::new(jid.to_string()),
        };
        self.chats.insert(0, chat);
        self.invalidate_chat_cache();
    }

    /// Drop the chats a complete load said were gone, now that nobody is
    /// looking at them.
    ///
    /// See [`Self::departed_chats`]. Deferred rather than skipped: the
    /// deletion is a fact from the moment the complete load arrived, and the
    /// only reason to keep the rows is that someone is reading them.
    ///
    /// Called from the render pass, straight after
    /// [`Self::note_visible_conversation`] and before anything reads the chat
    /// list — so it answers against *this* frame's visibility rather than the
    /// last one's. Every way of looking away is already accounted for there:
    /// Status, Settings, the viewer, another conversation, a phone going back
    /// to its list. Asking a frame earlier meant a mobile Back deferred the
    /// removal one last time and then drew the list with the stale row still
    /// in it, with no repaint scheduled to take it away.
    pub(crate) fn prune_departed_chats(&mut self) {
        if self.departed_chats.is_empty() {
            return;
        }
        let visible = self.visible_chat.clone();
        let gone: Vec<String> = self
            .departed_chats
            .iter()
            .filter(|jid| visible.as_deref() != Some(jid.as_str()))
            .cloned()
            .collect();
        if gone.is_empty() {
            return;
        }
        for jid in &gone {
            self.departed_chats.remove(jid);
        }
        self.chats.retain(|chat| !gone.contains(&chat.jid));
        {
            // Keyed by JID alone, so a recreated chat would otherwise inherit
            // the removed one's rows. See the same eviction on the load path.
            let mut cache = self.message_list_cache.borrow_mut();
            for jid in &gone {
                cache.remove(jid);
            }
        }
        // For the same reason, and it is the stronger one here: a paging
        // position outliving its chat is a conversation that never asks for
        // its history again.
        self.forget_chat_paging(&gone);
        self.forget_missing_selection();
        // The viewer names a chat and a message in it, and resolves them every
        // frame: one left open over a chat that has just gone draws nothing,
        // keeps the keyboard, and swallows the Escape meant to close it.
        if self
            .media_viewer
            .as_ref()
            .is_some_and(|viewer| gone.iter().any(|jid| jid == &viewer.jid))
        {
            self.media_viewer = None;
        }
        self.invalidate_chat_cache();
    }

    /// Drop a selection that no longer names a chat.
    ///
    /// The conversation pane resolves the selection every frame, so one left
    /// pointing at a deleted chat draws the empty state — and on a phone that
    /// is the whole screen, with the Back button belonging to a conversation
    /// that is not there.
    fn forget_missing_selection(&mut self) {
        if self
            .selected_chat
            .as_deref()
            .is_some_and(|jid| !self.chats.iter().any(|chat| chat.jid == jid))
        {
            self.selected_chat = None;
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

    /// Move a chat to where its `last_message_time` belongs, newest first.
    ///
    /// [`Self::move_chat_to_top`] is right for live traffic, where whatever
    /// bumped the chat arrived just now and so is the newest thing the window
    /// holds. A system notice is not always that: the held ones are replayed
    /// immediately after a history load, carrying whatever clock they arrived
    /// with, so one can advance its own conversation and still be older than
    /// another chat's head. Dropping it at index 0 would stand it above a
    /// strictly newer row in a list the sidebar draws newest-first.
    ///
    /// `None` sorts last, which is where `Reverse(last_message_time)` puts a
    /// chat with nothing in it.
    fn reposition_chat_by_time(&mut self, index: usize) {
        if index >= self.chats.len() {
            return;
        }
        let chat = self.chats.remove(index);
        let target = slot_newest_first(&self.chats, chat.last_message_time);
        self.chats.insert(target, chat);
        // Note: chat cache invalidation is handled by the caller
    }

    /// Get the chat list scroll handle
    pub fn chat_list_scroll(&self) -> &VirtualListScrollHandle {
        &self.chat_list_scroll
    }

    /// The conversation's list state.
    pub fn message_list(&self) -> &ListState {
        &self.message_list
    }

    /// Put the reader at the foot of the conversation.
    ///
    /// Rarely needed: the list is anchored at the bottom, so it is already
    /// there unless the reader scrolled away. This is for after *they* did
    /// something — sending, or finishing a recording — where following the
    /// message down is what they expect.
    fn scroll_to_last_message(&self) {
        let count = self.message_list.item_count();
        if count > 0 {
            self.message_list.scroll_to_reveal_item(count - 1);
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
            let stashed = self
                .selected_chat
                .clone()
                .filter(|_| !old.trim().is_empty());
            if let Some(prev) = stashed.clone() {
                self.drafts.insert(prev, old);
            }
            // The list draws a "Draft" preview off this map, and its own
            // guard counts messages — which neither of these changed. Without
            // saying so, the chat being left showed no draft and the one being
            // opened kept claiming one while its text was already back in the
            // composer.
            if stashed.is_some() || !restored.is_empty() {
                self.invalidate_chat_cache();
            }
        }
        // A search belongs to the conversation it was typed for, so leaving
        // that conversation closes it rather than carrying a query into a
        // chat it says nothing about.
        if self
            .conversation_search
            .as_ref()
            .is_some_and(|search| search.jid != jid)
        {
            self.conversation_search = None;
        }
        if self
            .media_viewer
            .as_ref()
            .is_some_and(|viewer| viewer.jid != jid)
        {
            self.media_viewer = None;
        }
        // And so does a reply. The draft is one shared field feeding one
        // shared composer, so a reply begun in A and sent from B quoted A's
        // message — putting A's text in front of B as the thing being
        // answered.
        if self.selected_chat.as_deref() != Some(jid.as_str()) && self.reply_to.is_some() {
            self.cancel_reply(cx);
        }
        self.selected_chat = Some(jid.clone());
        self.navigate_to_chat();

        self.keyboard_intent = open;
        if open == ChatOpen::ToCompose {
            // After `navigate_to_chat`, so on mobile the composer exists on the
            // panel being switched to rather than the one being left.
            self.ensure_input_area(window, cx);
            self.focus_composer(window, cx);
        }

        // One request, where this used to send receipts and a bounded chat
        // action separately: the daemon owns both, along with the boundary
        // that keeps a read from swallowing anything newer. All it needs from
        // here is the message this side is looking at.
        if let Some(chat) = self
            .find_chat(&jid)
            .filter(|c| c.unread_count > 0 || c.manually_unread)
        {
            match read_bound(chat) {
                ReadBound::Now(newest) => {
                    info!("Marking {} read", observe_str(&jid));
                    if let Some(client) = &self.client {
                        client.mark_chat_read(&jid, newest);
                    }
                }
                ReadBound::WhenLoaded => {
                    debug!(
                        "holding the read for {} until its messages arrive",
                        observe_str(&jid)
                    );
                    self.owed_reads.insert(jid.clone());
                }
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
    pub fn reset_and_pair_again(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.app_state = AppState::Loading;

        let asked = self
            .client
            .take()
            .inspect(Session::forget_session)
            .is_some();
        self.event_task.take();

        self.forget_account_state(window, cx);

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

        // A failure routes back to the error screen, where retry stays
        // available.
        self.reconnect_task = Some(cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            let connected = attach(cx).await;
            let _ = entity.update(cx, |app, cx| {
                match connected {
                    Ok((client, ui_rx)) => {
                        app.event_task = Some(Self::spawn_event_task(ui_rx, cx));
                        app.client = Some(client);
                    }
                    Err(e) => {
                        app.app_state = AppState::Error(format!("Failed to reach the daemon: {e}"));
                        // The error screen says "we'll keep trying", and this
                        // is the attempt that was doing the trying: the timer
                        // that fired it has already ended. Without arming the
                        // next one the promise stops being true after exactly
                        // one automatic retry, with the sentence still on
                        // screen.
                        app.schedule_retry(cx);
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
    /// [`front_end_id`] is what keeps them apart, and it also namespaces the
    /// media-cache file a voice note is staged in.
    fn next_local_id(prefix: &str) -> String {
        use portable_atomic::AtomicU64;
        use std::sync::atomic::Ordering;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        format!(
            "{prefix}_{}_{}_{}",
            front_end_id(),
            wacore::time::now_millis(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Send a message to the currently selected chat
    fn send_message(&mut self, text: &str, cx: &mut Context<Self>) {
        // Taken, not read: a reply is answered once. Leaving the draft in
        // place quoted the same message from every message that followed.
        let quoted = self.reply_to.take().map(QuotedMessage::from);
        self.send_quoted(text, quoted, cx);
    }

    /// Send `text`, quoting whatever the caller says it quotes.
    ///
    /// Split from [`send_message`](Self::send_message) so a retry can carry
    /// the *original* message's quote: retrying is presented as sending the
    /// failed message again, and the reply draft that produced it was
    /// consumed when it was first sent.
    fn send_quoted(&mut self, text: &str, quoted: Option<QuotedMessage>, cx: &mut Context<Self>) {
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
        client.send_message(&jid, text, local_id.clone(), quoted.clone());

        // Add to local chat immediately for responsiveness; the client renames
        // it to the real id via MessageIdAssigned.
        let mut msg = ChatMessage::new_outgoing(local_id, text.to_string());
        // The bubble shows its own quote from the moment it is drawn, the way
        // the reply will look once it comes back from the store.
        msg.quoted = quoted;
        if self.add_message_to_chat(&jid, msg) {
            self.scroll_to_last_message();
        }
        // The reply bar goes with the reply it composed.
        if let Some(input) = &self.input_area {
            input.update(cx, |view, cx| view.set_reply(None, cx));
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
                                app.audio = AudioHolder::None;
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
                crate::platform::sleep(std::time::Duration::from_millis(33)).await;

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
                        app.audio = AudioHolder::None;
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

        // A message landing in the conversation *on screen* is read
        // immediately: receipt out now, no badge (select_chat won't re-run to
        // send it). On screen, not merely selected — the selection outlives
        // the pane that shows it, so reading statuses, opening Settings or
        // walking the chat list on a phone would otherwise send a read
        // receipt for a message nobody had laid eyes on.
        let read_now =
            !message.is_from_me && self.visible_chat.as_deref() == Some(chat_jid.as_str());

        // Cache the sender's name if provided
        if let Some(ref name) = sender_name {
            self.name_cache.insert(message.sender.clone(), name.clone());
        }

        // For group chats — and the status broadcast, which is likewise a
        // stream of other people's rows — set sender_name for display.
        if (is_group || is_status) && !message.is_from_me {
            message.sender_name = sender_name
                .clone()
                .or_else(|| self.name_cache.get(&message.sender).cloned());
        }

        // Their message ends their typing, more reliably than `paused` does:
        // the peer that stopped composing is not obliged to say so, and a
        // sender whose message just arrived is definitively no longer
        // mid-word. Without this the header and the sidebar row went on
        // claiming they were typing for the whole 10-second TTL, underneath
        // the message they had already sent.
        if !message.is_from_me {
            self.presence.clear_composing(&chat_jid, &message.sender);
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
            let newest = self.find_chat(&chat_jid).and_then(newest_shared_message);
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
                // The session resolved this name the same way it resolves the
                // one on their bubbles, so the typing line under them says
                // the same thing — and the conversation keeps it, rather than
                // the line above it keeping it alone. A chat whose only word
                // about someone is that they are typing otherwise draws their
                // rows under a fallback JID while naming them overhead, which
                // is the same person under two names one line apart.
                //
                // Only when it is news, though: presence arrives on every
                // burst of keystrokes, and re-writing the participant map
                // walks the timeline.
                let name = match sender_name {
                    Some(name) => {
                        if self
                            .find_chat(&chat_jid)
                            .and_then(|chat| chat.participant_name(&sender_jid))
                            != Some(name.as_str())
                        {
                            self.name_cache.insert(sender_jid.clone(), name.clone());
                            if let Some(chat) = self.find_chat_mut(&chat_jid) {
                                chat.update_participant(sender_jid.clone(), name.clone());
                            }
                            // Rows that were waiting for this name have it now.
                            self.invalidate_message_cache(&chat_jid);
                        }
                        name
                    }
                    // No answer in this event: memory of earlier ones, and
                    // the JID is the honest last resort.
                    None => self
                        .find_chat(&chat_jid)
                        .and_then(|chat| chat.participant_name(&sender_jid))
                        .map(str::to_owned)
                        .or_else(|| self.name_cache.get(&sender_jid).cloned())
                        .unwrap_or_else(|| sender_jid.clone()),
                };
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

    /// A group changed, or something else happened *to* a chat.
    ///
    /// Inserted the way hydrated history is, not the way a message is: no
    /// unread bump. Nobody replies to "Ana changed the group name", and a
    /// badge for one would be a badge for nothing to read.
    ///
    /// The sidebar is the exception, and deliberately so: `preview_for` draws
    /// the last row, so a notice that *is* the last row is already the line
    /// the list shows. Leaving the head metadata behind then made the row
    /// disagree with itself — the new text under the previous message's
    /// clock, still sitting at the previous message's place in the list.
    ///
    /// The row is local to this window's session, like a call record: the
    /// store does not hold group notifications, so there is nothing to
    /// reload it from.
    fn handle_system_notice(
        &mut self,
        chat_jid: String,
        notice_id: String,
        at: chrono::DateTime<chrono::Utc>,
        notice: SystemNotice,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.chats.iter().position(|chat| chat.jid == chat_jid) else {
            // A notification for a chat this window has never loaded — which
            // is the *normal* order for the one that says you were added to a
            // group. Fabricating the chat around it would draw a conversation
            // with no messages and no name; dropping it loses the row for
            // good, because the store does not hold system notices and the
            // reload that brings the chat cannot bring this back. So it waits.
            debug!("holding a system notice for {}", observe_str(&chat_jid));
            self.pending_notices
                .entry(chat_jid)
                .or_default()
                .push((notice_id, at, notice));
            return;
        };
        let chat = &mut self.chats[index];
        if chat.messages.iter().any(|message| message.id == notice_id) {
            return;
        }
        let mut message = ChatMessage::new_incoming(notice_id, chat_jid.clone(), String::new());
        message.timestamp = at;
        message.is_read = true;
        message.system = Some(notice);
        chat.insert_history_message(message);
        // The head metadata, when this is now the newest thing in the
        // conversation. Not the unread count: see above.
        if chat.last_message_time.is_none_or(|last| at > last) {
            chat.last_message_time = Some(at);
            self.reposition_chat_by_time(index);
        }

        self.invalidate_message_cache(&chat_jid);
        // And the sidebar, when the notice is now the last thing in the
        // conversation: the list's preview is drawn from that row, while its
        // cache guard counts chats and cannot see one gaining a message.
        self.invalidate_chat_cache();
        cx.notify();
    }

    /// Put back the notices that arrived before their conversation did.
    ///
    /// Called wherever chats are installed. Only the ones whose chat is now
    /// here are taken, so a notice for a group this account is still syncing
    /// keeps waiting rather than being dropped a second time.
    fn flush_pending_notices(&mut self, cx: &mut Context<Self>) {
        if self.pending_notices.is_empty() {
            return;
        }
        let ready: Vec<String> = self
            .pending_notices
            .keys()
            .filter(|jid| self.find_chat(jid).is_some())
            .cloned()
            .collect();
        for jid in ready {
            let Some(held) = self.pending_notices.remove(&jid) else {
                continue;
            };
            for (notice_id, at, notice) in held {
                self.handle_system_notice(jid.clone(), notice_id, at, notice, cx);
            }
        }
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

/// The newest message in `chat` that the daemon can also name.
///
/// `MarkRead` is a claim about what the requester saw, and the daemon checks it
/// against the second its read would clear. A call record and a group notice
/// are written *here* — the daemon holds no messages and never put them in a
/// summary — so bounding the request with one names a message that is in no
/// boundary anywhere, and the read is refused. The chat is cleared locally all
/// the same, so no receipt goes out, nothing is persisted, and the next
/// hydration puts the badge back.
/// Whether a read for this chat can be bounded now.
///
/// The daemon checks a `MarkRead` against the second it would clear and
/// refuses one that names nothing while it knows messages of its own — so a
/// row painted from the daemon's snapshot, which carries a preview and no
/// messages, cannot be read the moment it is opened. A chat with nothing
/// behind it at all still can: that is the manually-unread case, which has no
/// message to name and has to stay clearable.
enum ReadBound {
    Now(Option<String>),
    WhenLoaded,
}

fn read_bound(chat: &Chat) -> ReadBound {
    match newest_shared_message(chat) {
        Some(newest) => ReadBound::Now(Some(newest)),
        // Nothing to name and nothing behind it, so there is no boundary for
        // the daemon to hold this against either.
        None if chat.last_message.is_none() => ReadBound::Now(None),
        None => ReadBound::WhenLoaded,
    }
}

fn newest_shared_message(chat: &Chat) -> Option<String> {
    chat.messages
        .iter()
        .rev()
        .find(|message| message.system.is_none())
        .map(|message| message.id.clone())
}

impl WhatsAppApp {
    /// One second, for the two things that change with no event behind them.
    ///
    /// A pairing code expires on a wall clock, and `theme.json` changes when a
    /// person saves it in an editor. Neither produces anything to react to, so
    /// without this the countdown sat at the second it was issued until an
    /// unrelated event repainted the window, and `reload_if_changed` — written
    /// to be polled — had no caller at all.
    ///
    /// One task rather than two, and it only runs while there is something to
    /// watch: it stops as soon as pairing ends, unless a theme file exists to
    /// keep watching.
    /// Start polling `theme.json`, whatever the connection is doing.
    ///
    /// The same heartbeat drives the pairing countdown, which is why it is
    /// armed from there too; this is the other reason it has to run.
    pub fn watch_theme_file(&mut self, cx: &mut Context<Self>) {
        self.ensure_heartbeat(cx);
    }

    fn ensure_heartbeat(&mut self, cx: &mut Context<Self>) {
        if self.heartbeat.is_some() {
            return;
        }
        self.heartbeat = Some(cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            loop {
                crate::platform::sleep(std::time::Duration::from_secs(1)).await;
                // A `stat` unless the stamp moved, which is why polling a
                // file a person edits by hand is affordable.
                let (theme_changed, watching) = cx.update(|cx| {
                    let changed = crate::theme::reload_if_changed(cx);
                    // The base font is the window's rem reference, so a
                    // reload that moves it leaves every frame already laid
                    // out measured against the old scale — tokens reporting
                    // one size into a layout built for another. Both
                    // interactive paths call `window.refresh()` for this;
                    // editing the file by hand deserves the same.
                    if changed {
                        cx.refresh_windows();
                    }
                    (changed, crate::theme::watches_a_file(cx))
                });

                let alive = entity.update(cx, |app, cx| {
                    let pairing = matches!(app.app_state, AppState::WaitingForPairing { .. });
                    if theme_changed {
                        app.adopt_reloaded_theme(cx);
                    }
                    if pairing || theme_changed {
                        cx.notify();
                    }
                    // The countdown stops mattering the moment pairing ends;
                    // the file does not.
                    pairing || watching
                });
                match alive {
                    Ok(true) => continue,
                    // Nothing left to tick, or the view is gone.
                    Ok(false) | Err(_) => break,
                }
            }
            let _ = entity.update(cx, |app, _| app.heartbeat = None);
        }));
    }
}

impl Focusable for WhatsAppApp {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.chat_list_focus.clone()
    }
}

impl Render for WhatsAppApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // First, because every dimension below resolves from the scale this
        // settles: the window's size is folded into the base font here and
        // nowhere else, which is what lets a 480×640 handheld get the whole
        // design at its own size without a single component learning that
        // small screens exist. See `theme::fit_to_viewport`.
        if crate::theme::fit_to_viewport(window.viewport_size(), cx) {
            // Frames already laid out against the previous scale are stale —
            // including the rem gpui-component's own controls resolve from.
            window.refresh();
        }
        let entity = cx.entity().clone();
        // Cleared here and set by whichever branch below actually draws a
        // conversation, so it describes this frame rather than an older one:
        // the pairing, error and Settings screens draw none. The keyboard
        // surfaces go the same way and for the same reason — focus may only
        // be handed to something this frame drew.
        self.visible_chat = None;
        self.keyboard_surfaces = KeyboardSurfaces::default();

        // Window-level commands hang off the root so they work wherever focus
        // happens to be, which is the point of a window-level command.
        let root = div()
            .size_full()
            // The window's own keyboard target. Every frame draws it, which
            // is what makes it somewhere focus can always be put — and what
            // brings the handlers below into the dispatch path, since a
            // focused handle that is not in the frame sends keys to gpui's
            // root and never reaches ours.
            .track_focus(&self.root_focus)
            // The call overlay is positioned against this box, because it
            // outlives the branch below: a phone ringing while Settings is
            // open is still a phone ringing.
            .relative()
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
            }))
            // The message actions land here rather than on the bubble that
            // raised them: a popup menu dispatches from its own focus, which
            // is inside the menu and not inside the row, and the root is the
            // one ancestor both paths share.
            .on_action(cx.listener(|app, reply: &ReplyToMessage, window, cx| {
                app.begin_reply(&reply.id, window, cx);
            }))
            .on_action(cx.listener(|app, retry: &RetryMessage, window, cx| {
                app.retry_send(&retry.id, window, cx);
            }))
            .on_action(|copy: &CopyMessage, _window, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.text.to_string()));
            });

        // Whether this frame draws the app rather than a screen on the way to
        // it, computed before the borrow below.
        let connected = matches!(self.app_state, AppState::Connected | AppState::Offline);

        let body = match &self.app_state {
            AppState::Loading => render_loading_view(cx).into_any_element(),
            AppState::Connecting => render_connecting_view(cx).into_any_element(),
            AppState::WaitingForPairing { qr_code, pair_code } => {
                render_pairing_view(qr_code.as_ref(), pair_code.clone(), cx).into_any_element()
            }
            AppState::Syncing => render_syncing_view(cx).into_any_element(),
            // Settings is a screen over the conversation view, so it takes
            // the whole frame while it is open rather than floating.
            AppState::Connected | AppState::Offline if self.settings.is_some() => {
                render_settings_view(self, window, cx).into_any_element()
            }
            AppState::Connected | AppState::Offline => {
                render_connected_view(self, window, cx).into_any_element()
            }
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

        // Outside the Settings-versus-conversation branch on purpose. The
        // card and the focus it takes were built by the conversation view
        // alone, so a call arriving while Settings was open rang at the far
        // end with nothing on screen to answer or refuse it — and no working
        // shortcut either — until the user happened to close Settings.
        let call_overlay = connected
            .then(|| render_call_overlay(self, window, cx))
            .flatten();

        // The card is the one surface the root draws itself, so it is the
        // one the root answers for.
        self.keyboard_surfaces.call_card = call_overlay.is_some();
        // After the body, which is what named the surfaces it drew, and
        // before the frame is painted, which is when the focus it hands over
        // takes effect. Unconditional: the screens on the way to a
        // conversation need a keyboard too, and the window is what they get.
        self.sync_overlay_focus(window, cx);

        root.child(body).children(call_overlay)
    }
}

/// Where a chat whose head is `at` belongs in a newest-first list that does
/// not contain it: the first slot whose neighbour is strictly older.
///
/// `None` is older than any timestamp, so an empty conversation lands at the
/// end — the same place `Reverse(last_message_time)` puts it.
fn slot_newest_first(rest: &[Chat], at: Option<chrono::DateTime<chrono::Utc>>) -> usize {
    rest.iter()
        .position(|other| other.last_message_time < at)
        .unwrap_or(rest.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> Option<chrono::DateTime<chrono::Utc>> {
        chrono::DateTime::from_timestamp(secs, 0)
    }

    fn chat(jid: &str, secs: Option<i64>) -> Chat {
        let mut chat = Chat::new(jid.to_string());
        chat.last_message_time = secs.and_then(at);
        chat
    }

    fn timeline_of(ids: &[&str]) -> MessageListCache {
        MessageListCache::new(&messages_of(ids), false, None)
    }

    /// Messages whose day comes from their *name*, not their position: the
    /// rows a page brings are the rows that chat always had, so `["m3","m4"]`
    /// and `["m1","m2","m3","m4"]` have to agree about m3 and m4 — dividers
    /// included, since a divider is a row like any other.
    fn messages_of(ids: &[&str]) -> Vec<ChatMessage> {
        ids.iter()
            .map(|id| {
                let mut message = ChatMessage::new_incoming(
                    (*id).to_string(),
                    "a@s.whatsapp.net".into(),
                    "olá".into(),
                );
                // One day apart, so every row also carries its own divider —
                // the rows a page brings are never only messages.
                let day: i64 = id.trim_start_matches('m').parse().unwrap();
                message.timestamp =
                    chrono::DateTime::from_timestamp(1_700_000_000 + day * 86_400, 0).unwrap();
                message
            })
            .collect()
    }

    fn anchored(jid: &str, rows: &MessageListCache, measured: MeasuredAgainst) -> TimelineAnchor {
        TimelineAnchor {
            jid: jid.to_string(),
            measured,
            rows: rows.clone(),
        }
    }

    fn same_layout() -> MeasuredAgainst {
        MeasuredAgainst {
            rem: 16.0,
            width: 600.0,
        }
    }

    /// A page of older history arrives in front of every message the list
    /// has measured — but *not* in front of the encryption notice, which
    /// holds index 0 whatever else happens. Spliced there rather than at the
    /// top, the reader stays where they were reading and every measured row
    /// keeps its own height; spliced at 0, the notice's height lands on a
    /// message. Reset, and a reader who scrolled back far enough to ask for
    /// more is thrown to the newest message for asking.
    #[test]
    fn a_page_of_older_history_is_spliced_after_the_notice() {
        let before = timeline_of(&["m3", "m4"]);
        let anchor = anchored("a@s.whatsapp.net", &before, same_layout());
        let after = timeline_of(&["m1", "m2", "m3", "m4"]);

        assert_eq!(
            timeline_sync(Some(&anchor), "a@s.whatsapp.net", &after, same_layout()),
            TimelineSync::Spliced {
                at: 1,
                removed: 0,
                added: after.items.len() - before.items.len(),
            }
        );
    }

    /// A page and an arrival between two frames. Both ends moved, so the
    /// stretch that changed spans everything between them — which is still
    /// one splice, and was a reset that threw the reader who had just asked
    /// for older history to the newest message.
    #[test]
    fn a_page_and_an_arrival_in_one_frame_are_one_splice() {
        let before = timeline_of(&["m3", "m4"]);
        let anchor = anchored("a@s.whatsapp.net", &before, same_layout());
        let after = timeline_of(&["m1", "m2", "m3", "m4", "m5"]);

        let TimelineSync::Spliced { at, removed, added } =
            timeline_sync(Some(&anchor), "a@s.whatsapp.net", &after, same_layout())
        else {
            panic!("both ends moved, and the middle is still the middle");
        };
        // The notice is common to both, and so is nothing after it once rows
        // arrived at either end.
        assert_eq!(at, 1);
        assert_eq!(removed, before.items.len() - 1);
        assert_eq!(added, after.items.len() - 1);
    }

    /// An arrival is still an append: the rows the list measured are all
    /// still where it measured them.
    #[test]
    fn an_arrival_is_still_spliced_at_the_end() {
        let before = timeline_of(&["m1", "m2"]);
        let anchor = anchored("a@s.whatsapp.net", &before, same_layout());
        let after = timeline_of(&["m1", "m2", "m3"]);

        assert_eq!(
            timeline_sync(Some(&anchor), "a@s.whatsapp.net", &after, same_layout()),
            TimelineSync::Spliced {
                at: before.items.len(),
                removed: 0,
                added: after.items.len() - before.items.len(),
            }
        );
    }

    /// The typing indicator is drawn after the last message and comes and
    /// goes on its own. Anchored on it, the last row is still the last row
    /// while a message lands in front of it — which reads as a page of older
    /// history, and put the arrival at the top of the conversation.
    #[test]
    fn a_message_landing_under_the_typing_row_is_still_an_arrival() {
        let typing = Some(TypingSummary {
            typists: vec![oxidezap_core::Typist {
                jid: "a@s.whatsapp.net".to_string(),
                name: "A".to_string(),
            }],
            total: 1,
            kind: oxidezap_core::ComposingKind::Text,
        });
        let before = MessageListCache::new(&messages_of(&["m1", "m2"]), false, typing.clone());
        let anchor = anchored("a@s.whatsapp.net", &before, same_layout());
        let after = MessageListCache::new(&messages_of(&["m1", "m2", "m3"]), false, typing);

        let TimelineSync::Spliced { at, removed, added } =
            timeline_sync(Some(&anchor), "a@s.whatsapp.net", &after, same_layout())
        else {
            panic!("a message arrived, whoever else is typing");
        };
        assert_eq!(removed, 0, "nothing left");
        assert_eq!(added, after.items.len() - before.items.len());
        // Straight after the last message, so the typing row is pushed down
        // rather than the arrival landing under it.
        assert_eq!(at, before.items.len() - 1);
    }

    /// Only a different conversation resets: within one, whatever changed is
    /// a stretch of rows, and the ones outside it are still what they were.
    #[test]
    fn only_another_conversation_is_reset() {
        let before = timeline_of(&["m1", "m2", "m3"]);
        let anchor = anchored("a@s.whatsapp.net", &before, same_layout());

        // Another conversation entirely.
        assert_eq!(
            timeline_sync(Some(&anchor), "b@s.whatsapp.net", &before, same_layout()),
            TimelineSync::Reset
        );
        // And nothing at all to compare against.
        assert_eq!(
            timeline_sync(None, "a@s.whatsapp.net", &before, same_layout()),
            TimelineSync::Reset
        );
    }

    /// A message that leaves — revoked and pruned, or a chat trimmed — is
    /// the same answer read the other way: the stretch it occupied is
    /// replaced by nothing.
    #[test]
    fn a_message_that_leaves_is_the_stretch_it_occupied() {
        let before = timeline_of(&["m1", "m2", "m3"]);
        let anchor = anchored("a@s.whatsapp.net", &before, same_layout());
        let after = timeline_of(&["m1", "m2"]);

        let TimelineSync::Spliced { removed, added, .. } =
            timeline_sync(Some(&anchor), "a@s.whatsapp.net", &after, same_layout())
        else {
            panic!("a row left, and the rest of them did not");
        };
        assert_eq!(removed - added, before.items.len() - after.items.len());
    }

    /// A rebuild with the rows unchanged is the other way a height goes
    /// stale: something inside a bubble grew. Nothing to splice, and nothing
    /// the list can be left believing either.
    #[test]
    fn a_rebuilt_row_is_remeasured_even_where_nothing_moved() {
        let before = timeline_of(&["m1", "m2"]);
        let anchor = anchored("a@s.whatsapp.net", &before, same_layout());
        // The same messages, built again — which is what an arriving image or
        // a landing reaction does.
        let rebuilt = timeline_of(&["m1", "m2"]);
        assert_ne!(rebuilt.build, before.build);

        assert_eq!(
            timeline_sync(Some(&anchor), "a@s.whatsapp.net", &rebuilt, same_layout()),
            TimelineSync::Remeasure
        );
        // And the same rows, not rebuilt, are nothing to say at all.
        assert_eq!(
            timeline_sync(Some(&anchor), "a@s.whatsapp.net", &before, same_layout()),
            TimelineSync::Nothing
        );
    }

    /// The same rows against a layout that has moved under them: every height
    /// is stale, and none of the rows are.
    #[test]
    fn a_resize_remeasures_rather_than_resets() {
        let rows = timeline_of(&["m1", "m2"]);
        let anchor = anchored("a@s.whatsapp.net", &rows, same_layout());
        let wider = MeasuredAgainst {
            rem: 16.0,
            width: 900.0,
        };

        assert_eq!(
            timeline_sync(Some(&anchor), "a@s.whatsapp.net", &rows, wider),
            TimelineSync::Remeasure
        );
        assert_eq!(
            timeline_sync(Some(&anchor), "a@s.whatsapp.net", &rows, same_layout()),
            TimelineSync::Nothing,
            "and a frame that changed nothing tells the list nothing"
        );
    }

    /// A row painted from the daemon's snapshot has a preview and no
    /// messages. Reading it the moment it is opened names nothing, and the
    /// daemon refuses a read that names nothing for a chat it holds messages
    /// for — so the badge would clear here and come back on the next
    /// hydration, with no receipt ever sent.
    #[test]
    fn a_row_without_its_messages_owes_its_read_to_the_load() {
        let mut row = chat("a@s.whatsapp.net", Some(10));
        row.last_message = Some("olá".to_string());
        assert!(matches!(read_bound(&row), ReadBound::WhenLoaded));

        row.messages.push(ChatMessage::new_incoming(
            "3EB0".into(),
            "a".into(),
            "olá".into(),
        ));
        assert!(
            matches!(read_bound(&row), ReadBound::Now(Some(id)) if id == "3EB0"),
            "a loaded row names the message it was looking at"
        );
    }

    /// A chat with nothing behind it has no boundary either, and must stay
    /// clearable: that is what marking a chat unread by hand produces.
    #[test]
    fn a_chat_with_nothing_behind_it_is_read_at_once() {
        assert!(matches!(
            read_bound(&chat("a@s.whatsapp.net", None)),
            ReadBound::Now(None)
        ));
    }

    #[test]
    fn the_newest_head_goes_first() {
        let rest = [chat("b", Some(30)), chat("c", Some(20))];
        assert_eq!(slot_newest_first(&rest, at(40)), 0);
    }

    #[test]
    fn a_head_older_than_another_chat_stays_under_it() {
        // The case a plain bump to index 0 got wrong: a held notice replayed
        // after a history load advances its own conversation and is still
        // older than the chat above it.
        let rest = [chat("b", Some(30)), chat("c", Some(10))];
        assert_eq!(slot_newest_first(&rest, at(20)), 1);
    }

    #[test]
    fn the_oldest_head_goes_last() {
        let rest = [chat("b", Some(30)), chat("c", Some(20))];
        assert_eq!(slot_newest_first(&rest, at(10)), 2);
    }

    /// `None` is below every `Some`, and the predicate is strict, so an empty
    /// conversation clears neither the dated chat nor the empty one already
    /// sitting there: it goes to the very end. That is the tie rule below,
    /// applied to two chats that are equally undated.
    #[test]
    fn an_empty_conversation_sorts_last_of_all() {
        let rest = [chat("b", Some(30)), chat("c", None)];
        assert_eq!(slot_newest_first(&rest, None), 2);
    }

    #[test]
    fn an_equal_head_keeps_the_incumbent_above_it() {
        let rest = [chat("b", Some(30)), chat("c", Some(10))];
        assert_eq!(slot_newest_first(&rest, at(30)), 1);
    }
}
