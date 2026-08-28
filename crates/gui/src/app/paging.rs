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

use std::collections::HashMap;

use gpui::Context;
use log::debug;
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
            Self::Loading { .. } | Self::Done { .. } => None,
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

impl WhatsAppApp {
    /// Ask for a conversation's newest page, if nobody has yet.
    ///
    /// Called where a conversation becomes the one on screen. The rows the
    /// attach load left are the unread tail and the preview — enough to draw a
    /// list row, not a conversation — so this is what fills the timeline.
    pub(super) fn ensure_timeline_page(&mut self, jid: &str) {
        let paging = self.timeline_pages.entry(jid.to_string()).or_default();
        if !matches!(paging, Paging::Unasked) {
            return;
        }
        // Only once there is somebody to answer. Nothing sends `PageLost`
        // for a request that was never made, so a `Loading` set here with no
        // session is one the list never leaves — and a reconnect keeps it,
        // because the paging state survives. Common on the web, where a page
        // cannot start a daemon and simply waits for one.
        let Some(client) = &self.client else {
            return;
        };
        *paging = Paging::Loading { from: None };
        client.load_messages(jid.to_string(), None);
    }

    /// Ask for the page before the one this conversation is showing.
    ///
    /// The reader is near the top of what has been drawn; whether there *is*
    /// anything older is what the last page said.
    pub(super) fn want_older_messages(&mut self, jid: &str) {
        let paging = self.timeline_pages.entry(jid.to_string()).or_default();
        let Some(Some(cursor)) = paging.to_ask() else {
            // Either nothing to ask for, or the first page is still coming —
            // and that one lands at the bottom, which is where the reader is.
            return;
        };
        // See `ensure_timeline_page`: no session, no request, so no
        // `Loading` to be stuck in.
        let Some(client) = &self.client else {
            return;
        };
        *paging = Paging::Loading {
            from: Some(cursor.clone()),
        };
        client.load_messages(jid.to_string(), Some(cursor));
    }

    /// Ask for more of the chat list.
    ///
    /// Normally from where the last history load stopped, which it says in the
    /// load itself (`note_chat_list_end`) — so the first ask is a page this
    /// window does not have. Asking from the top is the fallback for a load
    /// that named no position: it re-fetches the rows already on screen to
    /// obtain a cursor, and they merge into themselves.
    pub fn want_more_chats(&mut self) {
        let Some(ask) = self.chat_pages.to_ask() else {
            return;
        };
        let Some(client) = &self.client else {
            return;
        };
        self.chat_pages = Paging::Loading { from: ask.clone() };
        client.load_chats(ask);
    }

    /// Fold one page of a conversation into it.
    pub(super) fn apply_message_page(
        &mut self,
        jid: String,
        messages: Vec<ChatMessage>,
        next: Option<PageCursor>,
        cx: &mut Context<Self>,
    ) {
        // A page nobody is waiting for is a page from before an account
        // reset: `forget_paging` clears these positions, and the answer to a
        // request made under the old account can still be on the socket.
        // Folding it in would put that account's rows into this one's list.
        let Some(Paging::Loading { from }) = self.timeline_pages.get(&jid) else {
            debug!(
                "a page arrived for {}, which nobody asked for",
                observe_str(&jid)
            );
            return;
        };
        let asked_from = from.clone();
        self.timeline_pages
            .insert(jid.clone(), Paging::arrived(asked_from, next));
        if messages.is_empty() {
            return;
        }
        let Some(chat) = self.find_chat_mut(&jid) else {
            // The chat left while its page was in flight. The page describes
            // nothing on screen; the rows are the store's and will come back
            // with it.
            debug!("a page arrived for {}, which is gone", observe_str(&jid));
            return;
        };
        for message in messages {
            chat.insert_history_message(message);
        }
        // The rows moved, and the timeline's own measurements are keyed to
        // them: see `sync_timeline`, which is what turns this into a splice at
        // the front rather than a reset to the bottom.
        self.invalidate_message_cache(&jid);
        self.invalidate_chat_cache();
        cx.notify();
    }

    /// Fold one page of the chat list into it.
    pub(super) fn apply_chat_page(
        &mut self,
        chats: Vec<Chat>,
        next: Option<PageCursor>,
        cx: &mut Context<Self>,
    ) {
        // The same rule, and the one that matters most: this page's rows go
        // into the list whether or not anything else remembers them.
        let Paging::Loading { from } = &self.chat_pages else {
            debug!("a chat page arrived that nobody asked for");
            return;
        };
        self.chat_pages = Paging::arrived(from.clone(), next);
        if chats.is_empty() {
            return;
        }
        self.merge_chats(chats);
        self.invalidate_chat_cache();
        cx.notify();
    }

    /// A page that was refused. Put the position back so it can be asked for
    /// again; a view that stayed `Loading` would never ask anything again.
    pub(super) fn page_lost(&mut self, jid: Option<String>) {
        match jid {
            Some(jid) => {
                let paging = self.timeline_pages.entry(jid).or_default();
                *paging = paging.lost();
            }
            None => self.chat_pages = self.chat_pages.lost(),
        }
    }

    /// Forget where these conversations continued.
    ///
    /// Called where a chat leaves the list. The cursors describe positions in
    /// rows that are gone, and a JID is reused: a chat recreated under the
    /// same name would otherwise inherit a `Done` and never ask for its own
    /// history again — an empty conversation with nothing to fill it. Keyed
    /// by JID alone, like the message cache evicted beside it.
    pub(super) fn forget_chat_paging(&mut self, gone: &[String]) {
        for jid in gone {
            self.timeline_pages.remove(jid);
        }
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
    pub(super) fn reopen_finished_pages(&mut self, loaded: &[String]) {
        for jid in loaded {
            if let Some(paging) = self.timeline_pages.get_mut(jid) {
                *paging = paging.reopened();
            }
        }
        self.chat_pages = self.chat_pages.reopened();
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
    pub(super) fn note_chat_list_end(&mut self, complete: bool, next: Option<String>) {
        settle_chat_list_end(&mut self.chat_pages, complete, next);
    }

    /// Everything this window learned about where its lists continue.
    ///
    /// Dropped with the account: the cursors describe positions in one
    /// account's store, and the next account's rows are not behind them.
    pub(super) fn forget_paging(&mut self) {
        self.timeline_pages.clear();
        self.chat_pages = Paging::Unasked;
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
        let state = crate::components::new_timeline_state(12);

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
}
