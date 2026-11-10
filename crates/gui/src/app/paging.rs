//! History, asked for rather than pushed.
//!
//! The daemon publishes the chat list and the newest rows its own bookkeeping
//! needs; everything else a window draws, it asks for. Two lists page the same
//! way — a conversation backwards through its messages, the sidebar forwards
//! through its chats — so they share one state machine and one set of rules:
//! never two requests for the same page, never a request past the end, and
//! never a page dropped on the floor because the answer was a refusal.
//!
//! What a cursor *is* stays the daemon's business. This side holds the last
//! one it was given and hands it back.

use std::collections::{HashMap, HashSet};

use gpui::{App, Context};
use log::{debug, error};
use oxidezap_core::{Chat, ChatMessage};
use oxidezap_ipc::PageCursor;
use wacore_binary::jid::observe_str;

use super::WhatsAppApp;

/// How close to the end of the drawn rows is close enough to ask for more.
///
/// Rows, not pixels: the timeline measures itself and the sidebar does not, so
/// a distance in pixels would mean two different things. A screen of either is
/// well under ten rows, so this asks about a screen ahead of the reader.
pub(super) const LOOKAHEAD_ROWS: usize = 8;

/// Where a paged list continues, and whether it is asking.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) enum Paging {
    /// Nothing asked for yet.
    #[default]
    Unasked,
    /// A page is on its way. `from` is where it was asked from, so a refusal
    /// can put the position back rather than losing the thread.
    Loading { from: Option<PageCursor> },
    /// There is more, continuing here.
    More(PageCursor),
    /// Everything there is has arrived.
    ///
    /// `from` is where the last ask started, which is what makes this
    /// reopenable: a list can grow *behind* its end while a history sync is
    /// still committing batches, and asking again from there is asking for
    /// exactly what was not there the first time.
    Done { from: Option<PageCursor> },
    /// The daemon returned the cursor we just asked from. Do not ask it again
    /// until a history change gives this list a reason to retry.
    Stalled { from: PageCursor },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatPageArrival {
    Ignored,
    Applied,
    Restarted,
    Rejected,
}

/// Requests needed to hydrate both chat lists for an unresolved notification.
///
/// A click can race the initial active snapshot, while the target itself may
/// live only in the include-archived list. Neither list being done proves the
/// tag is absent from the other one, so the caller keeps the response pending
/// until both have reached their terminal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NotificationPagePlan {
    /// A request for the active list, if it was not already in flight/done.
    /// The inner `None` asks from the top; `Some(cursor)` continues a page.
    pub(super) active: Option<Option<PageCursor>>,
    /// The equivalent request for the include-archived list.
    pub(super) archived: Option<Option<PageCursor>>,
    /// True once the active list's complete hydration has arrived.
    pub(super) active_done: bool,
    /// True once the archived list's complete hydration has arrived.
    pub(super) archived_done: bool,
}

impl NotificationPagePlan {
    pub(super) fn done(&self) -> bool {
        self.active_done && self.archived_done
    }
}

impl Paging {
    /// The cursor to ask with, when asking is the right thing to do.
    ///
    /// `None` twice over, and the difference matters: `Unasked` means "from
    /// the newest", `Done` and `Loading` mean "do not ask at all".
    fn to_ask(&self) -> Option<Option<PageCursor>> {
        match self {
            Self::Unasked => Some(None),
            Self::More(cursor) => Some(Some(cursor.clone())),
            Self::Loading { .. } | Self::Done { .. } | Self::Stalled { .. } => None,
        }
    }

    /// What a page's own answer says about where the list continues, given
    /// where it was asked from.
    fn arrived(from: Option<PageCursor>, next: Option<PageCursor>) -> Self {
        next.map_or(Self::Done { from }, Self::More)
    }

    /// Ask again from where the list ended, because it may not end there any
    /// more.
    ///
    /// A history sync materializes its batches over minutes, and a reader who
    /// reached the end of a conversation — or of the chat list — before it
    /// finished was told, truthfully, that there was nothing behind it. The
    /// rows that arrive afterwards are older than everything fetched, so the
    /// cursor the last ask used is exactly where they are; without this they
    /// stayed unreachable until a restart.
    fn reopened(&self) -> Self {
        match self {
            Self::Done { from: Some(cursor) } => Self::More(cursor.clone()),
            Self::Done { from: None } => Self::Unasked,
            Self::Stalled { from } => Self::More(from.clone()),
            unsettled => unsettled.clone(),
        }
    }

    /// A request that did not come back. The position is what it was.
    fn lost(&self) -> Self {
        match self {
            Self::Loading { from: Some(cursor) } => Self::More(cursor.clone()),
            Self::Loading { from: None } => Self::Unasked,
            settled => settled.clone(),
        }
    }
}

/// Where both paged lists stand, and nothing else.
///
/// The first of the four clusters lifted off [`WhatsAppApp`] into an entity of
/// its own. What it owns is two positions; what it deliberately does not own
/// is the rows those positions describe, which are the app's — so every method
/// here answers "should this be asked for, and from where" and the app is what
/// asks. Nothing in it is drawn, so nothing in it notifies: an entity that
/// nobody observes and no view reads is one whose changes cost a window
/// exactly nothing, which is the cheapest end of the split.
pub(super) struct Pages {
    /// Where each conversation's timeline continues, and whether it is asking.
    timelines: TimelinePages,
    /// Where the chat list continues.
    chats: Paging,
    /// The include-archived list has a different ordering window and cursor.
    archived_chats: Paging,
    /// A history change while an archived page is in flight invalidates its
    /// ordering. Let the response settle, then start a fresh scan at the top.
    restart_archived_after_load: bool,
    /// A failed initial Status request waits for a deliberate retry, rather
    /// than starting another request from every render of the status view.
    status_page_failed: bool,
    /// Views confirmed by history while a status page is in flight. That page
    /// may have read its rows before the confirmations reached the store.
    status_confirmed_in_flight: HashSet<String>,
}

impl Pages {
    pub(super) fn new() -> Self {
        Self {
            timelines: TimelinePages::new(),
            chats: Paging::default(),
            archived_chats: Paging::default(),
            restart_archived_after_load: false,
            status_page_failed: false,
            status_confirmed_in_flight: HashSet::new(),
        }
    }

    fn chat_list(&mut self, archived: bool) -> &mut Paging {
        if archived {
            &mut self.archived_chats
        } else {
            &mut self.chats
        }
    }

    /// Whether a conversation's newest page is worth asking for, marking it
    /// asked if it is.
    ///
    /// Only ever true once per conversation: the first page is the one that
    /// fills a timeline the attach load left holding a preview.
    fn open_timeline(&mut self, jid: &str) -> bool {
        if jid == oxidezap_core::STATUS_BROADCAST_JID && self.status_page_failed {
            return false;
        }
        let paging = self.timelines.entry(jid.to_string()).or_default();
        if !matches!(paging, Paging::Unasked) {
            return false;
        }
        if jid == oxidezap_core::STATUS_BROADCAST_JID {
            self.status_confirmed_in_flight.clear();
        }
        *paging = Paging::Loading { from: None };
        true
    }

    /// The cursor the page *before* this conversation's oldest row is asked
    /// with, marking it asked.
    fn older(&mut self, jid: &str) -> Option<PageCursor> {
        let paging = self.timelines.entry(jid.to_string()).or_default();
        // Either nothing to ask for, or the first page is still coming — and
        // that one lands at the bottom, which is where the reader is.
        let Some(Some(cursor)) = paging.to_ask() else {
            return None;
        };
        if jid == oxidezap_core::STATUS_BROADCAST_JID {
            self.status_confirmed_in_flight.clear();
        }
        *paging = Paging::Loading {
            from: Some(cursor.clone()),
        };
        Some(cursor)
    }

    /// The cursor the next page of the chat list is asked with, marking it
    /// asked. The inner `None` is "from the top".
    fn more_chats(&mut self, archived: bool) -> Option<Option<PageCursor>> {
        let list = self.chat_list(archived);
        let ask = list.to_ask()?;
        *list = Paging::Loading { from: ask.clone() };
        Some(ask)
    }

    /// Advance both chat lists for a notification response. This is
    /// deliberately separate from the rendered filter: a click is an
    /// explicit request for one chat, even while the active list is shown.
    fn notification_page_plan(&mut self) -> NotificationPagePlan {
        let active = self.more_chats(false);
        let archived = self.more_chats(true);
        NotificationPagePlan {
            active,
            archived,
            active_done: matches!(self.chats, Paging::Done { .. }),
            archived_done: matches!(self.archived_chats, Paging::Done { .. }),
        }
    }

    /// Settle a conversation's position on the page that arrived, saying
    /// whether anything was waiting for it.
    ///
    /// A page nobody is waiting for is a page from before an account reset:
    /// [`Self::forget`] clears these positions, and the answer to a request
    /// made under the old account can still be on the socket. Folding it in
    /// would put that account's rows into this one's list.
    fn timeline_page_arrived(&mut self, jid: &str, next: Option<PageCursor>) -> bool {
        let Some(Paging::Loading { from }) = self.timelines.get(jid) else {
            return false;
        };
        let asked_from = from.clone();
        if jid == oxidezap_core::STATUS_BROADCAST_JID {
            self.status_page_failed = false;
        }
        if let (Some(from), Some(returned)) = (asked_from.as_ref(), next.as_ref())
            && from == returned
        {
            self.timelines
                .insert(jid.to_string(), Paging::Stalled { from: from.clone() });
            return true;
        }
        self.timelines
            .insert(jid.to_string(), Paging::arrived(asked_from, next));
        true
    }

    /// The same, for the chat list.
    fn chat_page_arrived(&mut self, archived: bool, next: Option<PageCursor>) -> ChatPageArrival {
        let Paging::Loading { from } = self.chat_list(archived) else {
            return ChatPageArrival::Ignored;
        };
        let asked_from = from.clone();
        if archived && std::mem::take(&mut self.restart_archived_after_load) {
            self.archived_chats = Paging::Unasked;
            return ChatPageArrival::Restarted;
        }
        if let (Some(from), Some(returned)) = (asked_from.as_ref(), next.as_ref())
            && from == returned
        {
            *self.chat_list(archived) = Paging::Stalled { from: from.clone() };
            return ChatPageArrival::Rejected;
        }
        *self.chat_list(archived) = Paging::arrived(asked_from, next);
        ChatPageArrival::Applied
    }

    /// A page that was refused. Put the position back so it can be asked for
    /// again; a list that stayed `Loading` would never ask anything again.
    /// Return whether a visible state changed and needs a notification.
    fn lost(&mut self, jid: Option<&str>, archived: bool) -> bool {
        match jid {
            Some(jid) => {
                let paging = self.timelines.entry(jid.to_string()).or_default();
                let status_failed = jid == oxidezap_core::STATUS_BROADCAST_JID
                    && matches!(paging, Paging::Loading { .. });
                *paging = paging.lost();
                if status_failed {
                    self.status_page_failed = true;
                    self.status_confirmed_in_flight.clear();
                }
                status_failed
            }
            None => {
                if archived && std::mem::take(&mut self.restart_archived_after_load) {
                    self.archived_chats = Paging::Unasked;
                    return true;
                }
                let list = self.chat_list(archived);
                *list = list.lost();
                false
            }
        }
    }

    /// Forget where these conversations continued. See
    /// [`WhatsAppApp::forget_chat_paging`].
    fn forget_chats(&mut self, gone: &[String]) {
        for jid in gone {
            self.timelines.remove(jid);
            if jid == oxidezap_core::STATUS_BROADCAST_JID {
                self.status_page_failed = false;
                self.status_confirmed_in_flight.clear();
            }
        }
    }

    /// Ask the finished lists again. See
    /// [`WhatsAppApp::reopen_finished_pages`].
    fn reopen(&mut self, loaded: &[String]) {
        for jid in loaded {
            if let Some(paging) = self.timelines.get_mut(jid) {
                *paging = paging.reopened();
            }
        }
        self.chats = self.chats.reopened();
        // Unlike the active list, no attach load refreshes the archived
        // list's first page. An archive change can insert a recent chat
        // before every cursor this window already crossed, so resuming from
        // the old end would never discover it. Restart from the top once any
        // in-flight answer has settled; a request already on the wire keeps
        // its position so an older response cannot be mistaken for a newer
        // one.
        if matches!(self.archived_chats, Paging::Loading { .. }) {
            self.restart_archived_after_load = true;
        } else {
            self.archived_chats = Paging::Unasked;
        }
    }

    /// Everything this window learned about where its lists continue.
    fn forget(&mut self) {
        self.timelines.clear();
        self.chats = Paging::Unasked;
        self.archived_chats = Paging::Unasked;
        self.restart_archived_after_load = false;
        self.status_page_failed = false;
        self.status_confirmed_in_flight.clear();
    }

    /// Keep confirmations only until the currently requested page settles.
    pub(super) fn note_status_confirmations(&mut self, agreed: &HashSet<String>) {
        if matches!(
            self.timelines.get(oxidezap_core::STATUS_BROADCAST_JID),
            Some(Paging::Loading { .. })
        ) {
            self.status_confirmed_in_flight
                .extend(agreed.iter().cloned());
        }
    }
}

/// Keep active-list removals from a complete history load, but replace the
/// archived-list debt with what this completed scan actually deferred. A JID
/// re-found by the scan no longer owes removal when its view closes.
fn reconcile_departures(
    departed: &mut HashSet<String>,
    active: &HashSet<&str>,
    seen: &HashSet<String>,
    archived_deferred: Vec<String>,
) {
    departed.retain(|jid| active.contains(jid.as_str()) && !seen.contains(jid));
    departed.extend(archived_deferred);
}

impl WhatsAppApp {
    /// An already open Status view also needs its first page after reconnect.
    pub fn ensure_status_page(&mut self, cx: &mut App) {
        self.ensure_timeline_page(oxidezap_core::STATUS_BROADCAST_JID, cx);
    }

    /// Whether the status feed has a page that can be requested or retried.
    pub fn status_can_load_more(&self, cx: &App) -> bool {
        self.client.is_some()
            && self
                .pages
                .read(cx)
                .timelines
                .get(oxidezap_core::STATUS_BROADCAST_JID)
                .is_none_or(|state| {
                    matches!(state, Paging::Stalled { .. }) || state.to_ask().is_some()
                })
    }

    /// Request one status page, retaining its cursor across a failed request.
    pub fn load_more_status(&mut self, cx: &mut App) {
        let jid = oxidezap_core::STATUS_BROADCAST_JID;
        if self.client.is_none() {
            return;
        }
        self.pages.update(cx, |pages, _| {
            pages.status_page_failed = false;
            if let Some(Paging::Stalled { from }) = pages.timelines.get(jid) {
                pages
                    .timelines
                    .insert(jid.to_string(), Paging::More(from.clone()));
            }
        });
        if self
            .pages
            .read(cx)
            .timelines
            .get(jid)
            .is_none_or(|state| matches!(state, Paging::Unasked))
        {
            self.ensure_timeline_page(jid, cx);
        } else {
            self.want_older_messages(jid, cx);
        }
    }

    /// Ask for a conversation's newest page, if nobody has yet.
    ///
    /// Called where a conversation becomes the one on screen. The rows the
    /// attach load left are the unread tail and the preview — enough to draw a
    /// list row, not a conversation — so this is what fills the timeline.
    pub(super) fn ensure_timeline_page(&mut self, jid: &str, cx: &mut App) {
        // Only once there is somebody to answer. Nothing sends `PageLost`
        // for a request that was never made, so a `Loading` set here with no
        // session is one the list never leaves — and a reconnect keeps it,
        // because the paging state survives. Common on the web, where a page
        // cannot start a daemon and simply waits for one. Asked before the
        // position moves, so a window with no session leaves it `Unasked`.
        let Some(client) = &self.client else {
            return;
        };
        if self.pages.update(cx, |pages, _| pages.open_timeline(jid)) {
            client.load_messages(jid.to_string(), None);
        }
    }

    /// Ask for the page before the one this conversation is showing.
    ///
    /// The reader is near the top of what has been drawn; whether there *is*
    /// anything older is what the last page said.
    pub(super) fn want_older_messages(&mut self, jid: &str, cx: &mut App) {
        // See `ensure_timeline_page`: no session, no request, so no
        // `Loading` to be stuck in.
        let Some(client) = &self.client else {
            return;
        };
        if let Some(cursor) = self.pages.update(cx, |pages, _| pages.older(jid)) {
            client.load_messages(jid.to_string(), Some(cursor));
        }
    }

    /// Whether the active destination is waiting for another sidebar page.
    pub fn chat_list_loading(&self, cx: &App) -> bool {
        let pages = self.pages.read(cx);
        let state = if self.destination == super::Destination::Chats
            && self.chat_filter == super::ChatFilter::Archived
        {
            &pages.archived_chats
        } else {
            &pages.chats
        };
        matches!(state, Paging::Loading { .. })
    }

    /// Ask for more of the chat list.
    ///
    /// Normally from where the last history load stopped, which it says in the
    /// load itself (`note_chat_list_end`) — so the first ask is a page this
    /// window does not have. Asking from the top is the fallback for a load
    /// that named no position: it re-fetches the rows already on screen to
    /// obtain a cursor, and they merge into themselves.
    pub fn want_more_chats(&mut self, cx: &mut App) {
        let Some(client) = &self.client else {
            return;
        };
        let archived = self.destination == super::Destination::Chats
            && self.chat_filter == super::ChatFilter::Archived;
        if let Some(ask) = self.pages.update(cx, |pages, _| pages.more_chats(archived)) {
            if archived && ask.is_none() {
                self.archived_scan.clear();
            }
            client.load_chats(ask, archived);
        }
    }

    /// Ask both chat lists for the next page on behalf of a system
    /// notification click. Unlike [`Self::want_more_chats`], this does not
    /// depend on the current sidebar filter or whether either list is visible.
    pub(super) fn request_notification_pages(
        &mut self,
        cx: &mut App,
    ) -> Option<NotificationPagePlan> {
        let Some(client) = &self.client else {
            return None;
        };
        let result = self
            .pages
            .update(cx, |pages, _| pages.notification_page_plan());
        if let Some(ask) = &result.active {
            client.load_chats(ask.clone(), false);
        }
        if let Some(ask) = &result.archived {
            if ask.is_none() {
                self.archived_scan.clear();
            }
            client.load_chats(ask.clone(), true);
        }
        Some(result)
    }

    /// Fold one page of a conversation into it.
    pub(super) fn apply_message_page(
        &mut self,
        jid: String,
        mut messages: Vec<ChatMessage>,
        next: Option<PageCursor>,
        cx: &mut Context<Self>,
    ) {
        let (arrived, confirmed) = self.pages.update(cx, |pages, _| {
            let arrived = pages.timeline_page_arrived(&jid, next);
            let confirmed = if arrived && jid == oxidezap_core::STATUS_BROADCAST_JID {
                std::mem::take(&mut pages.status_confirmed_in_flight)
            } else {
                HashSet::new()
            };
            (arrived, confirmed)
        });
        if !arrived {
            debug!(
                "a page arrived for {}, which nobody asked for",
                observe_str(&jid)
            );
            return;
        }
        if messages.is_empty() {
            if jid == oxidezap_core::STATUS_BROADCAST_JID {
                cx.notify();
            }
            return;
        }
        if jid == oxidezap_core::STATUS_BROADCAST_JID {
            // Only the rows in this page can confirm a local view. A page
            // read before a click may still arrive after that click.
            let agreed: HashSet<&str> = messages
                .iter()
                .filter(|m| m.is_read)
                .map(|m| m.id.as_str())
                .collect();
            self.watched_status
                .retain(|id| !agreed.contains(id.as_str()));
            for message in &mut messages {
                if !message.is_from_me
                    && (self.watched_status.contains(&message.id)
                        || confirmed.contains(&message.id))
                {
                    message.is_read = true;
                }
            }
        }
        // The status broadcast can be outside the reduced chat snapshot. Its
        // page is explicitly requested by the status view, so materialize a
        // store-backed row before merging the returned updates.
        if jid == oxidezap_core::STATUS_BROADCAST_JID && self.find_chat(&jid).is_none() {
            let status = Chat::from_store(jid.clone(), crate::tr!(Status).into(), 0);
            self.chats.push(std::sync::Arc::new(status));
        }
        let Some(chat) = self.find_chat_mut(&jid) else {
            // The chat left while its page was in flight. The page describes
            // nothing on screen; the rows are the store's and will come back
            // with it.
            debug!("a page arrived for {}, which is gone", observe_str(&jid));
            return;
        };
        let mut moved = false;
        for message in messages {
            moved |= chat.insert_history_message(message);
        }
        // A page that repeated rows this conversation already holds changed
        // nothing to draw. The daemon publishes a history load on every ack
        // and receipt, and a conversation whose whole history fit in one page
        // reopens to `Unasked` and asks for that page again — so invalidating
        // here rebuilt the rows and re-measured the timeline from scratch
        // every time a receipt landed.
        if !moved {
            if jid == oxidezap_core::STATUS_BROADCAST_JID {
                cx.notify();
            }
            return;
        }
        // Invalidation compares selected identities and order against either
        // the render cache or retained snapshots: a prepend clears stale order,
        // while a receipt-only update to an existing row preserves selection.
        self.invalidate_message_cache(&jid, cx);
        self.invalidate_chat_cache();
        cx.notify();
    }

    /// Fold one page of the chat list into it.
    pub(super) fn apply_chat_page(
        &mut self,
        chats: Vec<Chat>,
        next: Option<PageCursor>,
        archived: bool,
        cx: &mut Context<Self>,
    ) {
        let archived_scan_finished = archived && next.is_none();
        // The same rule, and the one that matters most: this page's rows go
        // into the list whether or not anything else remembers them.
        match self
            .pages
            .update(cx, |pages, _| pages.chat_page_arrived(archived, next))
        {
            ChatPageArrival::Ignored => {
                debug!("a chat page arrived that nobody asked for");
                return;
            }
            ChatPageArrival::Restarted => {
                // This answer was ordered before the history change. The
                // next request restarts the scan and clears its seen set.
                cx.notify();
                return;
            }
            ChatPageArrival::Rejected => {
                // This is not a complete scan. Do not merge the page or prune
                // archived chats as though the daemon had reached the end.
                error!("chat page returned a non-advancing cursor");
                return;
            }
            ChatPageArrival::Applied => {}
        }
        if chats.is_empty() {
            if archived_scan_finished {
                self.finish_archived_scan(cx);
            }
            return;
        }
        if archived {
            // Include-archived pages also carry active rows. Remember all of
            // them so a re-found JID cancels old deferred removal debt even
            // if its archive flag changed since the previous scan.
            self.archived_scan
                .extend(chats.iter().map(|chat| chat.jid.clone()));
        }
        // The same entrance a history load uses: a page that merely called
        // `merge_chats` left a notice for a group that arrives only by page
        // parked forever, and never armed the status tick for a broadcast
        // that arrived the same way. Nothing here says a status update was
        // watched — only a load knows that.
        self.install_chats(chats, &std::collections::HashSet::new(), cx);
        if archived_scan_finished {
            self.finish_archived_scan(cx);
        }
        cx.notify();
    }

    /// Prune archived rows only after their include-archived scan reached its
    /// final page. Before then, absence means merely "not loaded yet".
    fn finish_archived_scan(&mut self, cx: &mut Context<Self>) {
        let visible = self.visible_chat.clone();
        let mut deferred = Vec::new();
        let mut dropped = Vec::new();
        {
            let mut cache = self.message_list_cache.borrow_mut();
            self.chats.retain(|chat| {
                match super::survives_archived_scan(chat, &self.archived_scan, visible.as_deref()) {
                    super::Survival::Keep => true,
                    super::Survival::Defer => {
                        deferred.push(chat.jid.clone());
                        true
                    }
                    super::Survival::Drop => {
                        cache.remove(&chat.jid);
                        dropped.push(chat.jid.clone());
                        false
                    }
                }
            });
        }
        let active = self
            .chats
            .iter()
            .filter(|chat| !chat.archived)
            .map(|chat| chat.jid.as_str())
            .collect();
        reconcile_departures(
            &mut self.departed_chats,
            &active,
            &self.archived_scan,
            deferred,
        );
        if dropped.is_empty() {
            return;
        }
        self.forget_chat_paging(&dropped, cx);
        self.forget_missing_selection(cx);
        self.invalidate_chat_cache();
        cx.notify();
    }

    /// A page that was refused. Put the position back so it can be asked for
    /// again; a view that stayed `Loading` would never ask anything again.
    pub(super) fn page_lost(
        &mut self,
        jid: Option<String>,
        archived: bool,
        cx: &mut Context<Self>,
    ) {
        if self
            .pages
            .update(cx, |pages, _| pages.lost(jid.as_deref(), archived))
        {
            cx.notify();
        }
    }

    /// Forget where these conversations continued.
    ///
    /// Called where a chat leaves the list. The cursors describe positions in
    /// rows that are gone, and a JID is reused: a chat recreated under the
    /// same name would otherwise inherit a `Done` and never ask for its own
    /// history again — an empty conversation with nothing to fill it. Keyed
    /// by JID alone, like the message cache evicted beside it.
    pub(super) fn forget_chat_paging(&mut self, gone: &[String], cx: &mut App) {
        self.pages.update(cx, |pages, _| pages.forget_chats(gone));
    }

    /// Ask the finished lists again, because the store has more to give.
    ///
    /// Called on *any* history load, not only a complete one: a load is
    /// complete when it returned fewer chats than it asked for, so an account
    /// with a hundred of them never sees one, and gating this on that gated
    /// it out of existence for exactly the accounts that page. A history sync
    /// commits its batches over minutes, so a conversation that ended before
    /// the sync did did not really end there.
    ///
    /// Reopening costs nothing on its own: a settled end becomes a position
    /// again, and a position is only asked from when a reader is at that end
    /// of the list. Only settled ends move — a list still waiting on a page is
    /// untouched, and one with a cursor already asks for itself.
    ///
    /// `loaded` are the chats the load carried, which is what a scoped reload
    /// says changed. The chat list itself reopens either way: a load naming
    /// chats is one the store answered, and whether it could also have grown
    /// the list is not something the event says.
    pub(super) fn reopen_finished_pages(&mut self, loaded: &[String], cx: &mut App) {
        self.pages.update(cx, |pages, _| pages.reopen(loaded));
    }

    /// Where a history load leaves the chat list.
    ///
    /// The load walked the store's order itself, so it knows something no
    /// front end can infer: `next` is the position it stopped at, and a
    /// complete load is the whole list with nothing after it. Adopting that
    /// is what stops the first "load more" from re-fetching the page the
    /// window was handed — the attach load left no cursor, so the only way to
    /// obtain one was to ask for those rows again.
    ///
    /// A page already in flight is left alone: it asked from a position of
    /// its own and its answer is what settles the list.
    pub(super) fn note_chat_list_end(
        &mut self,
        complete: bool,
        next: Option<String>,
        cx: &mut App,
    ) {
        self.pages.update(cx, |pages, _| {
            settle_chat_list_end(&mut pages.chats, complete, next);
        });
    }

    /// Everything this window learned about where its lists continue.
    ///
    /// Dropped with the account: the cursors describe positions in one
    /// account's store, and the next account's rows are not behind them.
    pub(super) fn forget_paging(&mut self, cx: &mut App) {
        self.pages.update(cx, |pages, _| pages.forget());
    }
}

/// What a history load leaves the chat list's position at.
///
/// Apart from the app, because it is one decision about one field and the app
/// is only where that field is kept.
fn settle_chat_list_end(pages: &mut Paging, complete: bool, next: Option<String>) {
    // A page already in flight asked from a position of its own, and its
    // answer is what settles the list. A load landing in between says nothing
    // this side did not already ask about.
    if matches!(pages, Paging::Loading { .. }) {
        return;
    }
    match (complete, next) {
        // The store's whole list, so there is nothing behind it — and nothing
        // worth asking for, since asking returns these same rows. True however
        // far this window had paged: the list ends here.
        (true, _) => *pages = Paging::Done { from: None },
        // Where the *first* page ends, which is only news to a list that has
        // never asked. A window that has paged deeper is already past it, and
        // adopting it would walk it back — re-fetching pages it has merged,
        // once per history load, which during a sync is repeatedly.
        (false, Some(cursor)) if matches!(pages, Paging::Unasked) => {
            *pages = Paging::More(PageCursor::new(&cursor));
        }
        // A load that says nothing about the list — a scoped reload, or a
        // daemon that predates the cursor — and a load whose position this
        // window is already past. Both leave it where it is; for a window
        // that has never asked, that is "from the top".
        _ => {}
    }
}

/// Whether a list showing rows up to `visible_end` out of `drawn` is close
/// enough to its far end to ask for the next page.
///
/// For a list that grows downwards — the sidebar, which pages forwards
/// through the chats.
pub fn nearing_end(visible_end: usize, drawn: usize) -> bool {
    visible_end + LOOKAHEAD_ROWS >= drawn
}

/// Whether a list whose topmost drawn row is `visible_start` is close enough
/// to its beginning to ask for the page before it.
///
/// For a conversation, which pages backwards: the rows it wants next are the
/// ones above what it is showing.
pub(super) fn nearing_start(visible_start: usize) -> bool {
    visible_start <= LOOKAHEAD_ROWS
}

/// The same question, of the list a conversation actually uses.
///
/// A bottom-anchored list has no scroll position until somebody scrolls it,
/// and answers "which row is at the top" with the row *past the last one*
/// while it has none — its way of saying "pinned to the end". Read as a
/// position, that is as far from the start as a list can be, so a
/// conversation whose loaded rows do not fill the window never asked for a
/// second page: the reader had nowhere to scroll to say they wanted one.
///
/// `can_scroll` is that missing half. It is consulted only for a list nobody
/// has scrolled, because a reader who has one has a real position and it is
/// the whole answer — and because a page's own rows are unmeasured until they
/// are laid out, so a height asked in between would report a conversation
/// that still fits when it no longer does.
pub(super) fn timeline_nearing_start(visible_start: usize, rows: usize, can_scroll: bool) -> bool {
    nearing_start(visible_start) || (visible_start >= rows && !can_scroll)
}

/// Where each conversation's timeline continues.
pub(super) type TimelinePages = HashMap<String, Paging>;

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(at: &str) -> PageCursor {
        PageCursor::new(at)
    }

    /// [`WhatsAppApp::note_chat_list_end`] without a window: the decision is
    /// about one field, and the app is only where it is kept.
    fn settled_by(from: Paging, complete: bool, next: Option<&str>) -> Paging {
        let mut pages = from;
        settle_chat_list_end(&mut pages, complete, next.map(str::to_string));
        pages
    }

    /// An answer is only folded in while something is waiting for it: an
    /// account reset clears these positions, and the page it asked for can
    /// still be on its way.
    #[test]
    fn only_a_waiting_list_takes_a_page() {
        assert!(matches!(
            Paging::Loading { from: None },
            Paging::Loading { .. }
        ));
        for settled in [
            Paging::Unasked,
            Paging::Done { from: None },
            Paging::More(cursor("c1:-:9:a@s.whatsapp.net")),
        ] {
            assert!(!matches!(settled, Paging::Loading { .. }));
        }
    }

    #[test]
    fn active_and_archived_chat_lists_keep_independent_cursors() {
        let mut pages = Pages::new();

        assert_eq!(pages.more_chats(false), Some(None));
        assert_eq!(pages.more_chats(true), Some(None));
        assert_eq!(
            pages.chat_page_arrived(false, Some(cursor("active-next"))),
            ChatPageArrival::Applied
        );
        assert_eq!(
            pages.chat_page_arrived(true, Some(cursor("archive-next"))),
            ChatPageArrival::Applied
        );

        assert_eq!(pages.more_chats(false), Some(Some(cursor("active-next"))));
        assert_eq!(pages.more_chats(true), Some(Some(cursor("archive-next"))));
    }

    #[test]
    fn a_nonadvancing_chat_cursor_stalls_without_completing_the_scan() {
        for archived in [false, true] {
            let mut pages = Pages::new();
            assert_eq!(pages.more_chats(archived), Some(None));
            assert_eq!(
                pages.chat_page_arrived(archived, Some(cursor("same"))),
                ChatPageArrival::Applied
            );
            assert_eq!(pages.more_chats(archived), Some(Some(cursor("same"))));
            assert_eq!(
                pages.chat_page_arrived(archived, Some(cursor("same"))),
                ChatPageArrival::Rejected
            );
            assert!(matches!(
                pages.chat_list(archived),
                Paging::Stalled { from } if *from == cursor("same")
            ));
            assert_eq!(
                pages.more_chats(archived),
                None,
                "do not repeat the IPC ask"
            );

            let plan = pages.notification_page_plan();
            assert!(!plan.done(), "an incomplete scan cannot rule out a chat");
            assert!(!plan.active_done && !plan.archived_done);

            pages.reopen(&[]);
            assert_eq!(
                pages.more_chats(archived),
                if archived {
                    Some(None)
                } else {
                    Some(Some(cursor("same")))
                },
                "a history change may make the cursor usable again"
            );
        }
    }

    #[test]
    fn a_notification_advances_active_and_archived_pages_without_the_filter() {
        let mut pages = Pages::new();

        let first = pages.notification_page_plan();
        assert_eq!(first.active, Some(None));
        assert_eq!(first.archived, Some(None));
        assert!(!first.done());

        let in_flight = pages.notification_page_plan();
        assert_eq!(in_flight.active, None);
        assert_eq!(in_flight.archived, None);
        assert!(
            !in_flight.done(),
            "a click must not duplicate either request"
        );

        assert_eq!(
            pages.chat_page_arrived(true, Some(cursor("archive-next"))),
            ChatPageArrival::Applied
        );
        let archive_next = pages.notification_page_plan();
        assert_eq!(archive_next.active, None);
        assert_eq!(archive_next.archived, Some(Some(cursor("archive-next"))));
        assert!(!archive_next.done());

        assert_eq!(
            pages.chat_page_arrived(true, None),
            ChatPageArrival::Applied
        );
        let archive_done = pages.notification_page_plan();
        assert!(archive_done.archived_done);
        assert!(!archive_done.active_done);
        assert!(
            !archive_done.done(),
            "archived completion does not prove the active list was hydrated"
        );

        assert_eq!(
            pages.chat_page_arrived(false, Some(cursor("active-next"))),
            ChatPageArrival::Applied
        );
        let active_next = pages.notification_page_plan();
        assert_eq!(active_next.active, Some(Some(cursor("active-next"))));
        assert_eq!(active_next.archived, None);
        assert!(!active_next.done());

        assert_eq!(
            pages.chat_page_arrived(false, None),
            ChatPageArrival::Applied
        );
        let both_done = pages.notification_page_plan();
        assert!(both_done.active_done);
        assert!(both_done.archived_done);
        assert!(both_done.done(), "only both complete scans end the retry");
    }

    #[test]
    fn a_notification_page_loss_reopens_only_the_failed_chat_list() {
        let mut pages = Pages::new();
        let first = pages.notification_page_plan();
        assert_eq!(first.active, Some(None));
        assert_eq!(first.archived, Some(None));

        // The archived request failed while the active request is still in
        // flight. Only the archived cursor is put back, so retrying cannot
        // duplicate the active request.
        pages.lost(None, true);
        let archived_retry = pages.notification_page_plan();
        assert_eq!(archived_retry.active, None);
        assert_eq!(archived_retry.archived, Some(None));

        // The active request can fail independently and is then re-asked at
        // its own position as well.
        pages.lost(None, false);
        let active_retry = pages.notification_page_plan();
        assert_eq!(active_retry.active, Some(None));
        assert_eq!(active_retry.archived, None);
    }

    /// A load walks the store's order itself, so what it says about the end
    /// of the list beats anything a window could infer from the rows.
    #[test]
    fn a_load_says_where_the_chat_list_stands() {
        // The whole list: nothing behind it, so nothing to ask for.
        assert_eq!(
            settled_by(Paging::Unasked, true, None),
            Paging::Done { from: None }
        );
        // Stopped at its limit: the next page is the one after these rows,
        // which is the ask this whole field exists to save.
        assert_eq!(
            settled_by(Paging::Unasked, false, Some("c1:-:9:a@s.whatsapp.net")),
            Paging::More(cursor("c1:-:9:a@s.whatsapp.net"))
        );
        // A scoped reload, or a daemon that predates the cursor: it says
        // nothing about the list, so the position is what it was.
        assert_eq!(settled_by(Paging::Unasked, false, None), Paging::Unasked);
        assert_eq!(
            settled_by(Paging::More(cursor("c1:-:9:a@s.whatsapp.net")), false, None),
            Paging::More(cursor("c1:-:9:a@s.whatsapp.net"))
        );
    }

    /// The cursor a load carries is where its *first* page ends, so it is
    /// news only to a list that has not asked for anything. A reader who has
    /// paged deeper is already past it, and every history load carries it
    /// again — adopting it would walk them back to the first page, over and
    /// over, for the length of a history sync.
    #[test]
    fn a_load_does_not_walk_a_deeper_list_back() {
        let deeper = Paging::More(cursor("c1:-:300:z@s.whatsapp.net"));
        assert_eq!(
            settled_by(deeper.clone(), false, Some("c1:-:100:a@s.whatsapp.net")),
            deeper
        );
        // Reached the end already: the load's position is behind that too.
        let ended = Paging::Done {
            from: Some(cursor("c1:-:300:z@s.whatsapp.net")),
        };
        assert_eq!(
            settled_by(ended.clone(), false, Some("c1:-:100:a@s.whatsapp.net")),
            ended
        );
        // But a complete load is the whole list however deep the reader is.
        assert_eq!(settled_by(deeper, true, None), Paging::Done { from: None });
    }

    /// A page already asked for names its own continuation, and its answer is
    /// what settles the list. A load landing in between must not move the
    /// position out from under it.
    #[test]
    fn a_load_does_not_move_a_page_in_flight() {
        let waiting = Paging::Loading {
            from: Some(cursor("c1:-:9:a@s.whatsapp.net")),
        };
        assert_eq!(settled_by(waiting.clone(), true, None), waiting);
        assert_eq!(
            settled_by(waiting.clone(), false, Some("c1:-:1:b@s.whatsapp.net")),
            waiting
        );
    }

    /// The three states that mean different things about asking: never asked,
    /// asked and waiting, asked and answered with more.
    #[test]
    fn only_a_settled_position_is_asked_from() {
        assert_eq!(Paging::Unasked.to_ask(), Some(None));
        assert_eq!(
            Paging::More(cursor("m1:1:2")).to_ask(),
            Some(Some(cursor("m1:1:2")))
        );
        assert_eq!(Paging::Loading { from: None }.to_ask(), None);
        assert_eq!(Paging::Done { from: None }.to_ask(), None);
    }

    /// A refusal must leave the list able to ask again, at the same place.
    #[test]
    fn a_lost_page_leaves_its_position_behind() {
        let waiting = Paging::Loading {
            from: Some(cursor("m1:1:2")),
        };
        assert_eq!(waiting.lost(), Paging::More(cursor("m1:1:2")));
        assert_eq!(Paging::Loading { from: None }.lost(), Paging::Unasked);
        // A position that was never in flight is not moved by somebody else's
        // failure.
        assert_eq!(
            Paging::Done { from: None }.lost(),
            Paging::Done { from: None }
        );
    }

    /// The end of a list is a page with no cursor, not an empty page: a page
    /// can be empty and still have something behind it.
    #[test]
    fn a_page_without_a_cursor_ends_the_list() {
        assert_eq!(
            Paging::arrived(None, None),
            Paging::Done { from: None },
            "the first page, and the last"
        );
        assert_eq!(
            Paging::arrived(None, Some(cursor("c1:-:9:a@s.whatsapp.net"))),
            Paging::More(cursor("c1:-:9:a@s.whatsapp.net"))
        );
    }

    /// A list that ended while the store was still being written did not end
    /// there: asking again from where it stopped is asking for what was not
    /// there yet.
    #[test]
    fn a_finished_list_reopens_where_it_stopped() {
        let ended = Paging::arrived(Some(cursor("m1:9:2")), None);
        assert_eq!(ended.reopened(), Paging::More(cursor("m1:9:2")));
        // A first page that was also the last has nowhere to continue from,
        // so it starts over — the same ask, and the same merge.
        assert_eq!(
            Paging::Done { from: None }.reopened(),
            Paging::Unasked,
            "from the newest again"
        );
        // Anything still moving is left alone.
        let waiting = Paging::Loading { from: None };
        assert_eq!(waiting.reopened(), waiting);
        assert_eq!(
            Paging::More(cursor("m1:9:2")).reopened(),
            Paging::More(cursor("m1:9:2"))
        );
    }

    #[test]
    fn the_ask_comes_before_the_last_row_is_drawn() {
        assert!(nearing_end(95, 100), "a screen from the end asks");
        assert!(!nearing_end(10, 100), "the middle does not");
        assert!(nearing_end(0, 0), "an empty list is at its end");
    }

    /// The two answers the predicate above is built on, read off a real list
    /// state rather than off `gpui`'s documentation.
    ///
    /// A `Bottom` list has no scroll position until somebody scrolls it, and
    /// says so by naming the row *past* the last one — which is the whole
    /// reason the predicate cannot read the index alone. The second half is
    /// the same fact in pixels: nothing to scroll, so no offset to scroll to.
    /// Both are `gpui`'s behaviour rather than ours, which is exactly why
    /// they are worth pinning here: a list that began answering `0` for the
    /// first would turn this into a predicate that asks for history on every
    /// frame, and neither end of that is visible from our own code.
    #[test]
    fn an_unscrolled_timeline_names_the_row_past_its_last() {
        let state = crate::components::new_timeline_state(12, crate::theme::Metrics::default());

        assert_eq!(
            state.logical_scroll_top().item_ix,
            state.item_count(),
            "pinned to the end reads as the row after the last one"
        );
        assert_eq!(
            state.max_offset_for_scrollbar().y,
            gpui::px(0.),
            "and a list with nowhere to scroll offers no offset to scroll to"
        );
        assert!(
            timeline_nearing_start(
                state.logical_scroll_top().item_ix,
                state.item_count(),
                state.max_offset_for_scrollbar().y > gpui::px(0.),
            ),
            "which together is a conversation showing its whole head, and asking"
        );
    }

    #[test]
    fn a_timeline_that_cannot_scroll_is_at_its_start() {
        // What a bottom-anchored list reports while nobody has scrolled it:
        // the row past the last one.
        assert!(
            timeline_nearing_start(40, 40, false),
            "a conversation that fits the window has its whole head on screen"
        );
        assert!(
            !timeline_nearing_start(40, 40, true),
            "one that does not fit is pinned to its end, which is not its start"
        );
        assert!(
            timeline_nearing_start(3, 200, true),
            "a reader near the top asks whatever the height says"
        );
        assert!(
            !timeline_nearing_start(120, 200, true),
            "and the middle of a conversation does not"
        );
    }

    /// A conversation pages the other way: what it wants next is above what
    /// it is showing.
    #[test]
    fn a_conversation_asks_when_the_reader_nears_its_top() {
        assert!(nearing_start(0), "the top row on screen asks");
        assert!(!nearing_start(40), "the middle does not");
    }

    const CHAT: &str = "111@s.whatsapp.net";

    /// A conversation asks for its newest page exactly once. The second call
    /// is the frame after the first, and the frame after that: this is asked
    /// from the render pass, so "only once" is the whole of the rule.
    #[test]
    fn a_timeline_opens_once() {
        let mut pages = Pages::new();

        assert!(pages.open_timeline(CHAT), "nobody had asked");
        assert!(!pages.open_timeline(CHAT), "and the ask is still in flight");
        assert!(pages.timeline_page_arrived(CHAT, None));
        assert!(
            !pages.open_timeline(CHAT),
            "an answered conversation does not start over"
        );
    }

    /// Status uses the same bounded pages even when its chat was not in the
    /// initial snapshot; failures retain the cursor and loops are stopped.
    #[test]
    fn status_pages_continue_and_retry_without_repeating_a_cursor() {
        let mut pages = Pages::new();
        let jid = oxidezap_core::STATUS_BROADCAST_JID;
        assert!(pages.open_timeline(jid));
        assert!(pages.timeline_page_arrived(jid, Some(cursor("status-2"))));
        assert_eq!(pages.older(jid), Some(cursor("status-2")));
        pages.lost(Some(jid), false);
        assert_eq!(pages.older(jid), Some(cursor("status-2")));
        assert!(pages.timeline_page_arrived(jid, Some(cursor("status-2"))));
        assert_eq!(pages.older(jid), None);
        pages.reopen(&[jid.to_string()]);
        assert_eq!(pages.older(jid), Some(cursor("status-2")));
        assert!(pages.timeline_page_arrived(jid, None));
        assert_eq!(pages.older(jid), None);
    }

    #[test]
    fn failed_first_status_page_exposes_retry_without_automatic_request_loop() {
        let mut pages = Pages::new();
        let jid = oxidezap_core::STATUS_BROADCAST_JID;
        assert!(pages.open_timeline(jid));
        assert!(pages.lost(Some(jid), false), "the view must be notified");
        assert!(
            !pages.open_timeline(jid),
            "rendering must not immediately retry"
        );
        pages.status_page_failed = false; // The load-more action permits a retry.
        assert!(pages.open_timeline(jid));
        assert!(
            !pages.open_timeline(jid),
            "a retry is still a single in-flight request"
        );
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn a_stalled_status_cursor_offers_an_explicit_retry(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;
        let path = std::env::temp_dir().join(format!(
            "oxidezap-status-cursor-{}.sock",
            std::process::id()
        ));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let endpoint = oxidezap_ipc::Endpoint::connect_at(&path).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
            let app = cx.new(WhatsAppApp::new);
            app.update(cx, |app, cx| {
                app.client = Some(crate::session::Session::recording_test_session(endpoint));
                let jid = oxidezap_core::STATUS_BROADCAST_JID;
                assert!(app.pages.update(cx, |pages, _| pages.open_timeline(jid)));
                assert!(app.pages.update(cx, |pages, _| pages.timeline_page_arrived(jid, Some(cursor("older")))));
                assert_eq!(app.pages.update(cx, |pages, _| pages.older(jid)), Some(cursor("older")));
                assert!(app.pages.update(cx, |pages, _| pages.timeline_page_arrived(jid, Some(cursor("older")))));
                app.ensure_status_page(cx);
                assert!(matches!(app.pages.read(cx).timelines.get(jid), Some(Paging::Stalled { .. })), "rendering must not retry automatically");
                assert!(app.status_can_load_more(cx), "a stalled cursor should offer a manual retry");
                app.load_more_status(cx);
                assert!(matches!(app.pages.read(cx).timelines.get(jid), Some(Paging::Loading { from: Some(from) }) if *from == cursor("older")));
                assert!(!app.status_can_load_more(cx), "the retry is already in flight");
            });
        });
        std::fs::remove_file(path).unwrap();
    }

    #[gpui::test]
    fn stale_status_page_after_confirmed_history_keeps_view(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
            let app = cx.new(WhatsAppApp::new);
            app.update(cx, |app, cx| {
                let jid = oxidezap_core::STATUS_BROADCAST_JID;
                let stale = ChatMessage::new_incoming(
                    "seen".into(),
                    "1@s.whatsapp.net".into(),
                    "one".into(),
                );
                let unseen = ChatMessage::new_incoming(
                    "unseen".into(),
                    "1@s.whatsapp.net".into(),
                    "two".into(),
                );
                let mut status = Chat::from_store(jid.into(), "Status".into(), 0);
                status.messages.push(stale.clone());
                app.handle_event(
                    oxidezap_core::UiEvent::HistoryLoaded {
                        chats: vec![status.clone()],
                        complete: false,
                        next: None,
                    },
                    cx,
                );
                // The pending page has already captured `stale` before this view.
                assert!(app.pages.update(cx, |pages, _| pages.open_timeline(jid)));
                app.open_status("1@s.whatsapp.net".into(), cx);
                assert!(app.watched_status.contains("seen"));
                assert!(app.find_chat(jid).unwrap().messages[0].is_read);
                status.messages[0].is_read = true;
                app.handle_event(
                    oxidezap_core::UiEvent::HistoryLoaded {
                        chats: vec![status],
                        complete: false,
                        next: None,
                    },
                    cx,
                );
                assert!(!app.watched_status.contains("seen"));
                assert!(app.find_chat(jid).unwrap().messages[0].is_read);
                app.apply_message_page(jid.into(), vec![stale, unseen], None, cx);
                assert!(
                    app.find_chat(jid)
                        .unwrap()
                        .messages
                        .iter()
                        .find(|m| m.id == "seen")
                        .unwrap()
                        .is_read,
                    "a page captured before the view must not undo a confirmed view"
                );
                assert!(
                    !app.find_chat(jid)
                        .unwrap()
                        .messages
                        .iter()
                        .find(|m| m.id == "unseen")
                        .unwrap()
                        .is_read,
                    "an unrelated status must remain unseen"
                );
                assert!(app.pages.read(cx).status_confirmed_in_flight.is_empty());
            });
        });
    }

    #[test]
    fn confirmed_status_protection_is_discarded_when_page_is_lost_or_forgotten() {
        let jid = oxidezap_core::STATUS_BROADCAST_JID;
        let mut pages = Pages::new();
        let agreed = HashSet::from(["seen".to_string()]);
        pages.note_status_confirmations(&agreed);
        assert!(pages.status_confirmed_in_flight.is_empty());
        assert!(pages.open_timeline(jid));
        pages.note_status_confirmations(&agreed);
        assert_eq!(pages.status_confirmed_in_flight, agreed);
        assert!(pages.lost(Some(jid), false));
        assert!(pages.status_confirmed_in_flight.is_empty());
        pages.status_page_failed = false;
        assert!(pages.open_timeline(jid));
        pages.note_status_confirmations(&agreed);
        pages.forget_chats(&[jid.to_string()]);
        assert!(pages.status_confirmed_in_flight.is_empty());
        assert!(pages.open_timeline(jid));
        pages.note_status_confirmations(&agreed);
        pages.forget();
        assert!(pages.status_confirmed_in_flight.is_empty());
    }

    #[gpui::test]
    fn stale_status_page_preserves_a_view_made_after_the_query(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
            let app = cx.new(WhatsAppApp::new);
            app.update(cx, |app, cx| {
                let jid = oxidezap_core::STATUS_BROADCAST_JID;
                assert!(app.pages.update(cx, |pages, _| pages.open_timeline(jid)));
                let message = ChatMessage::new_incoming(
                    "seen".into(),
                    "1@s.whatsapp.net".into(),
                    "one".into(),
                );
                app.apply_message_page(
                    jid.into(),
                    vec![message.clone()],
                    Some(cursor("older")),
                    cx,
                );
                app.watched_status.insert("seen".into());
                app.find_chat_mut(jid).unwrap().messages[0].is_read = true;
                assert_eq!(
                    app.pages.update(cx, |pages, _| pages.older(jid)),
                    Some(cursor("older"))
                );
                app.apply_message_page(jid.into(), vec![message], None, cx);
                assert!(app.find_chat(jid).unwrap().messages[0].is_read);
                assert!(app.watched_status.contains("seen"));
                app.watched_status.insert("other".into());
                app.pages
                    .update(cx, |pages, _| pages.reopen(&[jid.to_string()]));
                assert_eq!(
                    app.pages.update(cx, |pages, _| pages.older(jid)),
                    Some(cursor("older"))
                );
                let mut confirmed = app.find_chat(jid).unwrap().messages[0].clone();
                confirmed.is_read = true;
                app.apply_message_page(jid.into(), vec![confirmed], None, cx);
                assert!(!app.watched_status.contains("seen"));
                assert!(app.watched_status.contains("other"));
            });
        });
    }

    /// A status page can create the missing broadcast row and merge a later
    /// bounded page without marking either update watched.
    #[gpui::test]
    fn status_pages_materialize_missing_broadcast_and_merge(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
            let app = cx.new(WhatsAppApp::new);
            app.update(cx, |app, cx| {
                let jid = oxidezap_core::STATUS_BROADCAST_JID;
                assert!(app.find_chat(jid).is_none());
                assert!(app.pages.update(cx, |pages, _| pages.open_timeline(jid)));
                let first =
                    ChatMessage::new_incoming("s1".into(), "1@s.whatsapp.net".into(), "one".into());
                app.apply_message_page(jid.into(), vec![first], Some(cursor("status-2")), cx);
                assert_eq!(app.find_chat(jid).unwrap().messages.len(), 1);
                assert_eq!(
                    app.pages.update(cx, |pages, _| pages.older(jid)),
                    Some(cursor("status-2"))
                );
                let second =
                    ChatMessage::new_incoming("s2".into(), "2@s.whatsapp.net".into(), "two".into());
                app.apply_message_page(jid.into(), vec![second], None, cx);
                assert_eq!(app.find_chat(jid).unwrap().messages.len(), 2);
                assert!(
                    app.find_chat(jid)
                        .unwrap()
                        .messages
                        .iter()
                        .all(|message| !message.is_read)
                );
            });
        });
    }

    /// Paging backwards asks from where the last page ended, and never twice
    /// from the same place.
    #[test]
    fn a_conversation_asks_once_per_position() {
        let mut pages = Pages::new();
        pages.open_timeline(CHAT);
        pages.timeline_page_arrived(CHAT, Some(cursor("m1:9:2")));

        assert_eq!(pages.older(CHAT), Some(cursor("m1:9:2")));
        assert_eq!(pages.older(CHAT), None, "that page is already coming");
        assert!(pages.timeline_page_arrived(CHAT, None));
        assert_eq!(pages.older(CHAT), None, "and there is nothing behind it");
    }

    /// A page nobody is waiting for is a page from before an account reset.
    /// Folding it in would put the old account's rows into this one's list,
    /// which is why the answer is a refusal rather than a merge.
    #[test]
    fn a_page_from_a_forgotten_account_is_refused() {
        let mut pages = Pages::new();
        pages.open_timeline(CHAT);
        assert!(pages.more_chats(false).is_some());

        pages.forget();

        assert!(!pages.timeline_page_arrived(CHAT, None));
        assert_eq!(
            pages.chat_page_arrived(false, None),
            ChatPageArrival::Ignored
        );
    }

    /// A refusal puts both lists back where they were asking from, so the
    /// reader who scrolls again asks again rather than waiting forever.
    #[test]
    fn a_refusal_leaves_both_lists_able_to_ask() {
        let mut pages = Pages::new();
        pages.open_timeline(CHAT);
        pages.timeline_page_arrived(CHAT, Some(cursor("m1:9:2")));
        pages.older(CHAT);
        pages.more_chats(false);

        pages.lost(Some(CHAT), false);
        pages.lost(None, false);

        assert_eq!(pages.older(CHAT), Some(cursor("m1:9:2")));
        assert_eq!(
            pages.more_chats(false),
            Some(None),
            "from the top, as before"
        );
    }

    /// A chat that left takes its position with it: a JID is reused, and a
    /// recreated conversation inheriting a `Done` is one that never asks for
    /// its own history.
    #[test]
    fn a_departed_chat_leaves_no_position_behind() {
        let mut pages = Pages::new();
        pages.open_timeline(CHAT);
        pages.timeline_page_arrived(CHAT, None);

        pages.forget_chats(&[CHAT.to_string()]);

        assert!(
            pages.open_timeline(CHAT),
            "the chat that came back asks like a new one"
        );
    }

    /// A history sync commits its batches over minutes, so a list that ended
    /// before the sync did did not really end there. Reopening moves only the
    /// settled ends; anything still in flight names its own continuation.
    #[test]
    fn a_finished_list_reopens_and_an_asking_one_does_not() {
        let mut pages = Pages::new();
        pages.open_timeline(CHAT);
        pages.timeline_page_arrived(CHAT, Some(cursor("m1:9:2")));
        pages.older(CHAT);
        pages.timeline_page_arrived(CHAT, None);
        pages.more_chats(false);

        pages.reopen(&[CHAT.to_string()]);

        assert_eq!(
            pages.older(CHAT),
            Some(cursor("m1:9:2")),
            "asking again from where it stopped"
        );
        assert_eq!(
            pages.more_chats(false),
            None,
            "and a chat list still waiting is left alone"
        );
    }

    #[test]
    fn an_archived_list_restarts_at_the_top_after_store_changes() {
        let mut pages = Pages::new();
        pages.more_chats(true);
        pages.chat_page_arrived(true, Some(cursor("archive-next")));
        pages.more_chats(true);
        pages.chat_page_arrived(true, None);

        pages.reopen(&[]);

        assert_eq!(
            pages.more_chats(true),
            Some(None),
            "a newly archived recent chat can sort before the old cursor"
        );
    }

    #[test]
    fn an_archived_load_in_flight_restarts_after_its_stale_answer() {
        let mut pages = Pages::new();
        assert_eq!(pages.more_chats(true), Some(None));
        pages.reopen(&[]);
        assert_eq!(pages.more_chats(true), None, "wait for the old answer");
        assert_eq!(
            pages.chat_page_arrived(true, None),
            ChatPageArrival::Restarted,
            "a stale terminal page must not finish the new scan"
        );
        assert_eq!(pages.more_chats(true), Some(None));
        assert_eq!(
            pages.chat_page_arrived(true, None),
            ChatPageArrival::Applied
        );
    }

    #[test]
    fn a_lost_archived_load_with_pending_restart_starts_at_top() {
        let mut pages = Pages::new();
        pages.more_chats(true);
        pages.chat_page_arrived(true, Some(cursor("old-next")));
        assert_eq!(pages.more_chats(true), Some(Some(cursor("old-next"))));
        pages.reopen(&[]);
        assert!(pages.lost(None, true), "a new scan should be scheduled");
        assert_eq!(pages.more_chats(true), Some(None));
    }

    #[test]
    fn a_new_archived_scan_cancels_old_deferred_removal() {
        let mut departed = HashSet::from([
            "active@s.whatsapp.net".to_string(),
            "restored@s.whatsapp.net".to_string(),
            "reactivated@s.whatsapp.net".to_string(),
        ]);
        reconcile_departures(
            &mut departed,
            &HashSet::from(["active@s.whatsapp.net", "reactivated@s.whatsapp.net"]),
            &HashSet::from([
                "restored@s.whatsapp.net".to_string(),
                "reactivated@s.whatsapp.net".to_string(),
            ]),
            Vec::new(),
        );
        assert_eq!(
            departed,
            HashSet::from(["active@s.whatsapp.net".to_string()])
        );
    }
}
