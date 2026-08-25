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
use oxidezap_core::{MediaType, STATUS_BROADCAST_JID, StatusFeed};

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
        // Leaving Status is leaving the update that was playing. Every other
        // way out of the reader stops its media; this one changed which panel
        // was drawn and left a video decoding and talking underneath the
        // conversation that replaced it.
        if self.destination == Destination::Status {
            self.leave_shown_status();
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

        let now = wacore::time::now_utc();
        let mut cache = self.status_feed_cache.borrow_mut();
        // Count alone cannot see an update lapsing: nothing was added or
        // removed, the clock simply passed it.
        if let Some((cached_count, feed)) = cache.as_ref()
            && *cached_count == count
            && feed.next_expiry().is_none_or(|when| now < when)
        {
            return feed.clone();
        }

        let feed = chat
            .map(|chat| StatusFeed::from_chat_at(chat, now))
            .unwrap_or_default();
        *cache = Some((count, feed.clone()));
        feed
    }

    /// Redraw when the next update on screen lapses.
    ///
    /// A status is the one thing in the window that changes with nothing
    /// happening: no message arrives to mark a ring watched-out, so without
    /// this the rail kept a badge and the list kept a row for updates that
    /// had already gone. One timer for the earliest of them, re-armed on each
    /// firing rather than one per update.
    pub(super) fn ensure_status_tick(&mut self, cx: &mut Context<Self>) {
        if self.status_tick.is_some() {
            return;
        }
        let Some(when) = self.status_feed().next_expiry() else {
            return;
        };
        // Saturating, and never zero: a lapse already past still needs one
        // turn of the loop to be noticed, and a negative duration would make
        // the timer fire in a tight loop.
        let wait = (when - wacore::time::now_utc())
            .to_std()
            .unwrap_or(std::time::Duration::ZERO)
            .max(std::time::Duration::from_secs(1));

        self.status_tick = Some(cx.spawn(async move |entity: gpui::WeakEntity<Self>, cx| {
            smol::Timer::after(wait).await;
            let _ = entity.update(cx, |app, cx| {
                app.status_tick = None;
                // Read before the feed is dropped: afterwards there is no
                // way to ask what was on screen.
                let shown = app.shown_status_message_id();
                // The feed rebuilds itself off the clock; this is what makes
                // anything ask it again.
                app.status_feed_cache.borrow_mut().take();
                app.invalidate_chat_cache();
                // An update that lapses while it is being watched takes its
                // decoder and its audio with it. Every other way out of the
                // reader stops the media; this one did not, so a video that
                // expired mid-play went on playing behind whatever the window
                // showed next.
                if let Some(id) = shown
                    && app.shown_status_message_id().as_deref() != Some(id.as_str())
                {
                    app.stop_status_media(Some(id));
                }
                app.ensure_status_tick(cx);
                cx.notify();
            });
        }));
    }

    pub fn status_pane(&self) -> &StatusPane {
        &self.status_pane
    }

    /// How many contacts have something unwatched, for the rail's badge.
    pub fn status_unseen(&self) -> usize {
        self.status_feed().unseen_authors()
    }

    pub fn open_status(&mut self, author: String, cx: &mut Context<Self>) {
        // Before the selection moves, because afterwards there is no way to
        // ask what was on screen. Opening an image or a text update does not
        // touch the player, so a video left this way went on playing.
        self.leave_shown_status();
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
        self.leave_shown_status();
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
        // Read before the index moves: what has to be stopped is the update
        // being left, not the one arriving.
        let leaving = self.shown_status_message_id();
        if self.status_pane.step(forward, author.count()) {
            self.stop_status_media(leaving);
            self.mark_shown_status_seen();
            self.fetch_shown_status(cx);
            cx.notify();
        }
    }

    /// Stop the update on screen, because the reader is about to stop showing
    /// it.
    ///
    /// Opening someone's status starts its video decoding and playing, and
    /// nothing else ever stops it: closing the reader only cleared the
    /// selection, so the frame task kept running and the audio kept going over
    /// whatever the window showed next.
    fn leave_shown_status(&mut self) {
        let shown = self.shown_status_message_id();
        self.stop_status_media(shown);
    }

    /// Stop playback if it belongs to `message_id`.
    ///
    /// Scoped rather than a bare `stop_current_media`: a voice note started in
    /// a conversation keeps playing while its listener browses, and leaving a
    /// status is no reason to cut it off.
    fn stop_status_media(&mut self, message_id: Option<String>) {
        if let Some(id) = message_id
            && self.active_media.message_id() == Some(id.as_str())
        {
            self.stop_current_media();
        }
    }

    /// Which update the reader is showing, whether or not it needs fetching.
    fn shown_status_message_id(&self) -> Option<String> {
        let author_jid = self.status_pane.author()?.to_string();
        let feed = self.status_feed();
        let author = feed.author(&author_jid)?;
        let at = self.status_pane.index_in(author.count());
        feed.updates_of(author).nth(at).map(|m| m.id.clone())
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

    /// Fetch the update on screen, if its bytes are not here.
    ///
    /// A status arrives as a thumbnail at most, and unlike a conversation
    /// there is no bubble to tap: opening someone's status *is* the request to
    /// see it. One at a time — the one being looked at — rather than the whole
    /// run, because a run is watched one update at a time and the rest may
    /// never be reached.
    ///
    /// Video is fetched the same way. It used to be skipped here, which is why
    /// a video status sat on "cannot be shown" having never asked for the
    /// bytes that would show it.
    fn fetch_shown_status(&mut self, cx: &mut Context<Self>) {
        let Some((message_id, downloadable, kind)) = self.shown_status_media() else {
            return;
        };
        if self.is_downloading(&message_id) {
            return;
        }
        match kind {
            MediaType::Image => self.download_image(message_id, downloadable, cx),
            // The video path downloads *and* starts decoding, which is what
            // produces the frame the pane draws.
            MediaType::Video => self.toggle_video(message_id, downloadable, cx),
            other => log::debug!("a status update of type {other:?} has nothing to show"),
        }
    }

    /// The update on screen, when it has bytes worth fetching.
    ///
    /// `None` once they are here, or when there is nothing to fetch: a text
    /// status, or media the server gave no way to download.
    fn shown_status_media(&self) -> Option<(String, oxidezap_core::DownloadableMedia, MediaType)> {
        let author_jid = self.status_pane.author()?.to_string();
        let feed = self.status_feed();
        let author = feed.author(&author_jid)?;
        let at = self.status_pane.index_in(author.count());
        let message = feed.updates_of(author).nth(at)?;
        let media = message.media.as_ref()?;
        if !media.data.is_empty() && !media.data_is_preview {
            return None;
        }
        Some((
            message.id.clone(),
            media.downloadable.clone()?,
            media.media_type.clone(),
        ))
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
