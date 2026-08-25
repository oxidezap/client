//! Status has its own place in the window.
//!
//! The broadcast is not a conversation, so it is not in the list of them: the
//! sidebar has a destination for it, and picking a contact there opens their
//! updates in the pane the conversation would otherwise occupy.
//!
//! Only the *selection* lives here. What there is to select is derived from
//! the broadcast's messages by [`oxidezap_core::StatusFeed`], rebuilt when
//! they change rather than mirrored into a second copy that could disagree
//! with the first.

use gpui::Context;
use oxidezap_core::{STATUS_BROADCAST_JID, StatusFeed};

use super::WhatsAppApp;

/// Which of the sidebar's destinations is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Destination {
    #[default]
    Chats,
    Status,
}

impl Destination {
    pub const ALL: [Self; 2] = [Self::Chats, Self::Status];

    pub fn label(self) -> &'static str {
        match self {
            Self::Chats => "Chats",
            Self::Status => "Status",
        }
    }

    /// Stable per-destination element id, so a rebuilt rail keeps its buttons.
    pub fn id(self) -> &'static str {
        match self {
            Self::Chats => "nav-chats",
            Self::Status => "nav-status",
        }
    }
}

/// Whose updates are open, and which one of them.
#[derive(Debug, Clone, Default)]
pub struct StatusPane {
    /// The author's JID; the empty string is our own updates, which is why
    /// this is not simply `Option<String>` keyed by contact.
    author: Option<String>,
    /// Position within that author's run. Kept in range by the reader below
    /// rather than trusted: the run grows while it is open.
    index: usize,
}

impl StatusPane {
    pub fn author(&self) -> Option<&str> {
        self.author.as_deref()
    }

    pub fn is_open(&self) -> bool {
        self.author.is_some()
    }

    /// Open `author` at their first update. Reopening the same author starts
    /// them over, which is what tapping their row again means.
    pub fn open(&mut self, author: String) {
        self.author = Some(author);
        self.index = 0;
    }

    pub fn close(&mut self) {
        self.author = None;
        self.index = 0;
    }

    /// Where the reader is, clamped to a run of `len`. A run only ever grows
    /// at the end, so clamping is enough to survive one arriving mid-view.
    pub fn index_in(&self, len: usize) -> usize {
        self.index.min(len.saturating_sub(1))
    }

    /// Step within the run. Returns whether it moved: at either end the caller
    /// leaves the view alone rather than repainting for nothing.
    pub fn step(&mut self, forward: bool, len: usize) -> bool {
        let at = self.index_in(len);
        let next = if forward {
            (at + 1).min(len.saturating_sub(1))
        } else {
            at.saturating_sub(1)
        };
        if next == self.index {
            return false;
        }
        self.index = next;
        true
    }
}

impl WhatsAppApp {
    pub fn destination(&self) -> Destination {
        self.destination
    }

    /// Switch destinations. The selection inside each is left as it was, so
    /// coming back to Chats lands on the conversation that was open.
    pub fn set_destination(&mut self, destination: Destination, cx: &mut Context<Self>) {
        if self.destination == destination {
            return;
        }
        self.destination = destination;
        cx.notify();
    }

    /// The broadcast, grouped by author.
    ///
    /// Cached against the message count and dropped by the same invalidation
    /// that drops the chat list's: one pass over the updates per change, not
    /// one per frame.
    pub fn status_feed(&self) -> StatusFeed {
        let chat = self.chats.iter().find(|chat| chat.is_status);
        let count = chat.map_or(0, |chat| chat.messages.len());

        let mut cache = self.status_feed_cache.borrow_mut();
        if let Some((cached_count, feed)) = cache.as_ref()
            && *cached_count == count
        {
            return feed.clone();
        }

        let feed = chat.map(StatusFeed::from_chat).unwrap_or_default();
        *cache = Some((count, feed.clone()));
        feed
    }

    pub fn status_pane(&self) -> &StatusPane {
        &self.status_pane
    }

    /// How many contacts have something unwatched, for the rail's badge.
    pub fn status_unseen(&self) -> usize {
        self.status_feed().unseen_authors()
    }

    pub fn open_status(&mut self, author: String, cx: &mut Context<Self>) {
        self.status_pane.open(author);
        self.navigate_to_chat();
        self.mark_shown_status_seen();
        self.fetch_shown_status(cx);
        cx.notify();
    }

    pub fn close_status(&mut self, cx: &mut Context<Self>) {
        if !self.status_pane.is_open() {
            return;
        }
        self.status_pane.close();
        cx.notify();
    }

    /// Move within the open author's run.
    pub fn step_status(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(author) = self.status_pane.author().map(str::to_string) else {
            return;
        };
        let feed = self.status_feed();
        let Some(author) = feed.author(&author) else {
            return;
        };
        if self.status_pane.step(forward, author.count()) {
            self.mark_shown_status_seen();
            self.fetch_shown_status(cx);
            cx.notify();
        }
    }

    /// Fetch the update on screen, if its bytes are not here.
    ///
    /// A status arrives as a thumbnail at most, and unlike a conversation
    /// there is no bubble to tap: opening someone's status *is* the request to
    /// see it. One at a time — the one being looked at — rather than the
    /// whole run, because a run is watched one update at a time and the rest
    /// may never be reached.
    fn fetch_shown_status(&mut self, cx: &mut Context<Self>) {
        let Some(author_jid) = self.status_pane.author().map(str::to_string) else {
            return;
        };
        let feed = self.status_feed();
        let Some(author) = feed.author(&author_jid) else {
            return;
        };
        let at = self.status_pane.index_in(author.count());
        let Some(message) = feed.updates_of(author).nth(at) else {
            return;
        };
        let Some(media) = message.media.as_ref() else {
            return;
        };
        // Only what the pane can actually draw: a video would download and
        // still show the placeholder.
        if media.media_type != oxidezap_core::MediaType::Image || !media.data.is_empty() {
            return;
        }
        let Some(downloadable) = media.downloadable.clone() else {
            return;
        };
        let message_id = message.id.clone();
        if self.is_downloading(&message_id) {
            return;
        }
        self.download_image(message_id, downloadable, cx);
    }

    /// Mark the update currently on screen as watched.
    ///
    /// Locally, and only the one being looked at. WhatsApp's own read receipt
    /// for a status is a privacy setting the library does not expose, so this
    /// is the honest half of it: the ring stops claiming there is something
    /// new *here*, and nothing is told to anyone. A store reload resets it,
    /// which is the right way round — the store is what actually knows.
    fn mark_shown_status_seen(&mut self) {
        let Some(author_jid) = self.status_pane.author().map(str::to_string) else {
            return;
        };
        let feed = self.status_feed();
        let Some(author) = feed.author(&author_jid) else {
            return;
        };
        let at = self.status_pane.index_in(author.count());
        let Some(message_id) = feed.updates_of(author).nth(at).map(|m| m.id.clone()) else {
            return;
        };

        let Some(chat) = self.chats.iter_mut().find(|chat| chat.is_status) else {
            return;
        };
        let Some(message) = chat
            .messages
            .iter_mut()
            .find(|message| message.id == message_id)
        else {
            return;
        };
        if message.is_read {
            return;
        }
        message.is_read = true;
        self.invalidate_chat_cache();
    }

    /// Whether the broadcast is the chat with this JID, so the parts of the
    /// app that walk conversations can leave it alone.
    pub fn is_status_jid(jid: &str) -> bool {
        jid == STATUS_BROADCAST_JID
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_of_one_has_nowhere_to_step() {
        let mut pane = StatusPane::default();
        pane.open("a@s.whatsapp.net".to_string());
        assert!(!pane.step(true, 1));
        assert!(!pane.step(false, 1));
        assert_eq!(pane.index_in(1), 0);
    }

    #[test]
    fn stepping_stops_at_both_ends() {
        let mut pane = StatusPane::default();
        pane.open("a@s.whatsapp.net".to_string());
        assert!(pane.step(true, 3));
        assert!(pane.step(true, 3));
        assert!(!pane.step(true, 3));
        assert_eq!(pane.index_in(3), 2);
        assert!(pane.step(false, 3));
        assert!(pane.step(false, 3));
        assert!(!pane.step(false, 3));
    }

    #[test]
    fn a_run_that_shrank_does_not_read_past_its_end() {
        let mut pane = StatusPane::default();
        pane.open("a@s.whatsapp.net".to_string());
        pane.step(true, 5);
        pane.step(true, 5);
        assert_eq!(pane.index_in(2), 1);
        assert_eq!(pane.index_in(0), 0);
    }

    #[test]
    fn reopening_an_author_starts_their_run_over() {
        let mut pane = StatusPane::default();
        pane.open("a@s.whatsapp.net".to_string());
        pane.step(true, 4);
        pane.open("a@s.whatsapp.net".to_string());
        assert_eq!(pane.index_in(4), 0);
    }

    #[test]
    fn our_own_updates_are_a_selection_like_any_other() {
        let mut pane = StatusPane::default();
        pane.open(String::new());
        assert!(pane.is_open());
        assert_eq!(pane.author(), Some(""));
        pane.close();
        assert!(!pane.is_open());
    }
}
