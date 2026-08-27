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
use whatsapp_rust::wacore_binary::jid::observe_str;

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
    Done,
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
            Self::Loading { .. } | Self::Done => None,
        }
    }

    /// What a page's own answer says about where the list continues.
    fn arrived(next: Option<PageCursor>) -> Self {
        next.map_or(Self::Done, Self::More)
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
        *paging = Paging::Loading { from: None };
        if let Some(client) = &self.client {
            client.load_messages(jid.to_string(), None);
        }
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
        *paging = Paging::Loading {
            from: Some(cursor.clone()),
        };
        if let Some(client) = &self.client {
            client.load_messages(jid.to_string(), Some(cursor));
        }
    }

    /// Ask for more of the chat list.
    ///
    /// The first ask is deliberately from the top: the attach load handed this
    /// window the first page without a cursor to continue from, so the page
    /// that re-asks for it is what produces one. Its rows are the ones already
    /// on screen and merge into them; every page after it is new.
    pub fn want_more_chats(&mut self) {
        let Some(ask) = self.chat_pages.to_ask() else {
            return;
        };
        self.chat_pages = Paging::Loading { from: ask.clone() };
        if let Some(client) = &self.client {
            client.load_chats(ask);
        }
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
        if !matches!(self.timeline_pages.get(&jid), Some(Paging::Loading { .. })) {
            debug!(
                "a page arrived for {}, which nobody asked for",
                observe_str(&jid)
            );
            return;
        }
        self.timeline_pages
            .insert(jid.clone(), Paging::arrived(next));
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
        if !matches!(self.chat_pages, Paging::Loading { .. }) {
            debug!("a chat page arrived that nobody asked for");
            return;
        }
        self.chat_pages = Paging::arrived(next);
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

    /// Everything this window learned about where its lists continue.
    ///
    /// Dropped with the account: the cursors describe positions in one
    /// account's store, and the next account's rows are not behind them.
    pub(super) fn forget_paging(&mut self) {
        self.timeline_pages.clear();
        self.chat_pages = Paging::Unasked;
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

/// Where each conversation's timeline continues.
pub(super) type TimelinePages = HashMap<String, Paging>;

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(at: &str) -> PageCursor {
        PageCursor::new(at)
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
            Paging::Done,
            Paging::More(cursor("c1:-:9:a@s.whatsapp.net")),
        ] {
            assert!(!matches!(settled, Paging::Loading { .. }));
        }
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
        assert_eq!(Paging::Done.to_ask(), None);
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
        assert_eq!(Paging::Done.lost(), Paging::Done);
    }

    /// The end of a list is a page with no cursor, not an empty page: a page
    /// can be empty and still have something behind it.
    #[test]
    fn a_page_without_a_cursor_ends_the_list() {
        assert_eq!(Paging::arrived(None), Paging::Done);
        assert_eq!(
            Paging::arrived(Some(cursor("c1:-:9:a@s.whatsapp.net"))),
            Paging::More(cursor("c1:-:9:a@s.whatsapp.net"))
        );
    }

    #[test]
    fn the_ask_comes_before_the_last_row_is_drawn() {
        assert!(nearing_end(95, 100), "a screen from the end asks");
        assert!(!nearing_end(10, 100), "the middle does not");
        assert!(nearing_end(0, 0), "an empty list is at its end");
    }

    /// A conversation pages the other way: what it wants next is above what
    /// it is showing.
    #[test]
    fn a_conversation_asks_when_the_reader_nears_its_top() {
        assert!(nearing_start(0), "the top row on screen asks");
        assert!(!nearing_start(40), "the middle does not");
    }
}
