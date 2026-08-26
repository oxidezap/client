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
    /// The update that position resolved to, so the reader can be put back
    /// on it after a rebuild moved it.
    ///
    /// A position alone was safe only while a run grew at the end, and it
    /// does not: a live update and a hydrated one can both be stamped before
    /// the one being watched, and the same index then silently resolves to a
    /// different message — never marked watched, never fetched, with the
    /// previous update's video still playing over it.
    shown: Option<String>,
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
        self.shown = None;
    }

    pub fn close(&mut self) {
        self.author = None;
        self.index = 0;
        self.shown = None;
    }

    /// The update the reader is anchored to, if it has resolved one.
    pub fn shown(&self) -> Option<&str> {
        self.shown.as_deref()
    }

    /// Record what the current position resolved to.
    pub fn follow(&mut self, message_id: Option<String>) {
        self.shown = message_id;
    }

    /// Put the reader back on the update it was showing, which has moved to
    /// `index`.
    pub fn seek(&mut self, index: usize) {
        self.index = index;
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

/// Mark every update in `watched` read, and answer with how many needed it.
///
/// Never one of ours: `is_read` on a row from us is the peer's read tick, and
/// nothing local may set it. Nothing should put one in the set either — this
/// is the second lock on the same door.
fn apply_watched(
    chat: &mut oxidezap_core::Chat,
    watched: &std::collections::HashSet<String>,
) -> usize {
    let mut marked = 0;
    for message in &mut chat.messages {
        if message.is_from_me || message.is_read || !watched.contains(&message.id) {
            continue;
        }
        message.is_read = true;
        marked += 1;
    }
    marked
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
        let Some(when) = self.status_feed().next_expiry() else {
            return;
        };
        // An armed timer is not necessarily the right one. An update that
        // arrives out of order, or a history load that brings an older run in
        // behind a newer one, can put the *earliest* deadline behind the one
        // already waited on — and the timer that fires late leaves the lapsed
        // update in the list, in the ring and in the badge until the later
        // one expires. Re-armed only when the answer moved earlier, so the
        // ordinary case still costs one comparison.
        if self.status_tick.is_some() && self.status_tick_at.is_some_and(|armed| armed <= when) {
            return;
        }
        // Saturating, and never zero: a lapse already past still needs one
        // turn of the loop to be noticed, and a negative duration would make
        // the timer fire in a tight loop.
        let wait = (when - wacore::time::now_utc())
            .to_std()
            .unwrap_or(std::time::Duration::ZERO)
            .max(std::time::Duration::from_secs(1));

        self.status_tick_at = Some(when);
        self.status_tick = Some(cx.spawn(async move |entity: gpui::WeakEntity<Self>, cx| {
            smol::Timer::after(wait).await;
            let _ = entity.update(cx, |app, cx| {
                app.status_tick = None;
                app.status_tick_at = None;
                // Read out of the feed the reader is actually holding, not
                // by asking for a fresh one. This fires *because* a deadline
                // passed, which is exactly when `status_feed` re-derives
                // against the clock — so asking it here answers with the
                // successor, or with nothing, and the comparison below then
                // sees no transition at all: the expiring video went on
                // playing behind a closed reader, and its successor was
                // neither marked watched nor fetched.
                let holding = app
                    .status_feed_cache
                    .borrow()
                    .as_ref()
                    .map(|(_, feed)| feed.clone());
                let shown = holding.as_ref().and_then(|feed| app.shown_status_in(feed));
                // The feed rebuilds itself off the clock; this is what makes
                // anything ask it again.
                app.status_feed_cache.borrow_mut().take();
                app.invalidate_chat_cache();
                // An update that lapses while it is being watched takes its
                // decoder and its audio with it, and hands the reader over to
                // whatever is behind it — which is a change of what is on
                // screen like any other, not merely a stop.
                if let Some(id) = shown
                    && app.shown_status_message_id().as_deref() != Some(id.as_str())
                {
                    app.shown_status_changed(Some(id), cx);
                }
                // And when the whole run lapses there is nothing to show at
                // all. The reader draws an empty state with no way out of it
                // — on a phone the pane *is* the screen and its Back button
                // belongs to the conversation view — so the pane closes
                // rather than stranding whoever was watching.
                if app.status_pane.is_open() && app.shown_status_message_id().is_none() {
                    app.close_status(cx);
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
        // Read before the selection moves, because afterwards there is no way
        // to ask what was on screen. Opening an image or a text update does
        // not touch the player, so a video left this way went on playing.
        let leaving = self.shown_status_message_id();
        self.status_pane.open(author);
        self.navigate_to_chat();
        self.shown_status_changed(leaving, cx);
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
            self.shown_status_changed(leaving, cx);
            cx.notify();
        }
    }

    /// The reader is showing a different update than it was.
    ///
    /// Three things that only ever happen together: stop what is being left,
    /// mark what has arrived as watched, and fetch its bytes. Every way the
    /// shown update changes goes through here — which is what expiry did not,
    /// so an update that lapsed under the reader handed its place to the next
    /// one and left it unfetched and still ringed as new.
    fn shown_status_changed(&mut self, leaving: Option<String>, cx: &mut Context<Self>) {
        self.stop_status_media(leaving);
        self.mark_shown_status_seen();
        self.fetch_shown_status(cx);
        // Four things, then: the reader is anchored to the update it arrived
        // at rather than to the place it sits in the run. See
        // [`Self::reconcile_status_pane`].
        let arrived = self.shown_status_message_id();
        self.status_pane.follow(arrived);
    }

    /// Keep the reader on the update it is showing, whatever a rebuild did to
    /// the order.
    ///
    /// The pane holds a position, and clamping it was enough only while a run
    /// grew at the end. It does not: an update delivered live and one brought
    /// in by a history load can both be stamped before the one being watched,
    /// and inserting either ahead of it makes the same index a different
    /// message — with nothing said, so it was neither marked watched nor
    /// fetched, and the update it replaced went on playing over it.
    ///
    /// Driven from the render pass, like the overlay focus it sits beside:
    /// the feed is derived from messages that arrive from the daemon, and
    /// this is where the answer is about to be drawn.
    pub fn reconcile_status_pane(&mut self, cx: &mut Context<Self>) {
        let Some(anchor) = self.status_pane.shown().map(str::to_string) else {
            return;
        };
        let Some(author_jid) = self.status_pane.author().map(str::to_string) else {
            return;
        };
        let feed = self.status_feed();
        let Some(author) = feed.author(&author_jid) else {
            // Their whole run is gone — the last of it lapsed, or the only
            // update they had was taken back. Same as an anchor that is no
            // longer in the run, except that there is nothing behind it
            // either: stop what was playing and close the reader, which would
            // otherwise be an empty state with no way out of it on a phone.
            self.status_pane.follow(None);
            self.stop_status_media(Some(anchor));
            self.close_status(cx);
            return;
        };
        match feed.updates_of(author).position(|m| m.id == anchor) {
            // Still in the run: follow it, wherever it moved to.
            Some(at) => self.status_pane.seek(at),
            // Gone — revoked, or lapsed between one frame and the next. The
            // position now resolves to a different update, which is a change
            // like any other and has to be announced as one.
            None => {
                self.status_pane.follow(None);
                self.shown_status_changed(Some(anchor), cx);
            }
        }
    }

    /// Stop the update on screen, because the reader is about to stop showing
    /// it.
    ///
    /// Opening someone's status starts its video decoding and playing, and
    /// nothing else ever stops it: closing the reader only cleared the
    /// selection, so the frame task kept running and the audio kept going over
    /// whatever the window showed next.
    pub(super) fn leave_shown_status(&mut self) {
        let shown = self.shown_status_message_id();
        self.stop_status_media(shown);
    }

    /// Stop playback if it belongs to `message_id`, and drop a fetch that was
    /// going to start it.
    ///
    /// Scoped rather than a bare `stop_current_media`: a voice note started in
    /// a conversation keeps playing while its listener browses, and leaving a
    /// status is no reason to cut it off.
    ///
    /// The pending request is the other half. A status video that is still
    /// downloading owns nothing yet — no player, no `active_media` — and its
    /// completion autoplays on the strength of `pending_media_request` alone,
    /// so leaving during the download started a video behind whatever the
    /// window showed next. Both halves are the same act of leaving, so they
    /// are the same method.
    fn stop_status_media(&mut self, message_id: Option<String>) {
        let Some(id) = message_id else {
            return;
        };
        if self.active_media.message_id() == Some(id.as_str()) {
            self.stop_current_media();
        }
        if self.pending_media_request.as_deref() == Some(id.as_str()) {
            self.pending_media_request = None;
        }
    }

    /// Which update the reader is showing, whether or not it needs fetching.
    fn shown_status_message_id(&self) -> Option<String> {
        self.shown_status_in(&self.status_feed())
    }

    /// The same question, asked of a feed the caller already has.
    ///
    /// Split out for the expiry tick, which needs the answer as of the feed
    /// that was on screen — the one this firing is about to invalidate.
    fn shown_status_in(&self, feed: &StatusFeed) -> Option<String> {
        let author_jid = self.status_pane.author()?;
        let author = feed.author(author_jid)?;
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
        // Ours are never unseen — the feed does not count them and no ring
        // is drawn over them — and `is_read` on a row from us means the peer
        // read it. Marking one here falsifies that tick, and remembering it
        // would have `apply_watched` re-falsify it after every load.
        if message.is_from_me || message.is_read {
            return;
        }
        message.is_read = true;
        // Remembered as well as set, in two places that answer different
        // questions. This window's own set survives a hydration merge, which
        // replaces these rows from the store and would otherwise put the ring
        // straight back before the daemon has been heard from. The daemon's
        // copy survives the window: it owns the store, and a view that lived
        // only here died with the process — which is why every restart
        // offered updates that had already been watched as new.
        self.watched_status.insert(message_id.clone());
        if let Some(client) = &self.client {
            client.mark_status_watched(vec![message_id]);
        }
        self.invalidate_chat_cache();
    }

    /// Take back views the daemon did not record.
    ///
    /// The ring is *not* forced back on here, because a refusal is not proof
    /// that nothing was written. The store's flush contract is temporal — a
    /// batch somebody else's write dropped is reported to whoever flushed
    /// next — so a view can be refused and stored, and another window may
    /// have stored the same one anyway. Writing `is_read = false` over that
    /// would replace durable truth with a guess, and the refused write raises
    /// no invalidation to correct it.
    ///
    /// So this drops the claim and asks for the history again. The reload
    /// answers both cases with the same fact: watched updates come back
    /// watched, and one that was never written comes back new.
    pub(super) fn forget_status_views(&mut self, message_ids: &[String], cx: &mut Context<Self>) {
        let mut taken_back = false;
        for id in message_ids {
            taken_back |= self.watched_status.remove(id);
        }
        if !taken_back {
            return;
        }
        if let Some(client) = &self.client {
            client.reload_history();
        }
        self.invalidate_chat_cache();
        cx.notify();
    }

    /// Put the locally watched updates back after a hydration merge.
    ///
    /// The daemon is told too, and answers a later reload with the row
    /// already read — but not this one: a hydration merge in flight was
    /// assembled before the view was recorded. This is what keeps "watched"
    /// from flickering back on in between.
    ///
    /// `agreed` names the updates *this load* brought back already read. Those
    /// are the claims worth dropping and the only ones: a load of some other
    /// chat says nothing about these updates, and neither does one whose page
    /// stopped short of an older update still on screen — the row survives the
    /// merge carrying the `is_read` this window wrote, which afterwards looks
    /// exactly like one the store agreed about.
    pub(super) fn restore_watched_status(&mut self, agreed: &std::collections::HashSet<String>) {
        if self.watched_status.is_empty() {
            return;
        }
        // Dropped first: a claim the store now makes for itself is not one
        // this window has to keep making, and holding every id it ever
        // watched is a set that only grows.
        self.watched_status.retain(|id| !agreed.contains(id));
        let watched = &self.watched_status;
        let Some(chat) = self.chats.iter_mut().find(|chat| chat.is_status) else {
            return;
        };
        apply_watched(chat, watched);
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
        let Some((message_id, media)) = self.shown_status_media() else {
            return;
        };
        if self.is_downloading(&message_id) {
            return;
        }
        let Some(downloadable) = media.downloadable else {
            log::debug!("the update on screen offers no way to fetch it");
            return;
        };
        // Whether the bytes are here is the *image's* question. A picture
        // that has arrived is drawn and there is nothing else to do.
        let needs_bytes = media.data.is_empty() || media.data_is_preview;
        match media.media_type {
            MediaType::Image if needs_bytes => self.download_image(message_id, downloadable, cx),
            MediaType::Image => {}
            // A video needs a *player*, and that is a different thing from
            // needing bytes: `toggle_video` downloads when it must and starts
            // decoding either way, and the frames it produces are what the
            // pane draws. Skipping it once the bytes were here left a watched
            // video reopening on its poster frame with nothing to start it.
            MediaType::Video => self.toggle_video(message_id, downloadable, cx),
            other => log::debug!("a status update of type {other:?} has nothing to show"),
        }
    }

    /// The update on screen and its media, whatever state that media is in.
    ///
    /// `None` for a text status, which has none.
    fn shown_status_media(&self) -> Option<(String, oxidezap_core::MediaContent)> {
        let author_jid = self.status_pane.author()?.to_string();
        let feed = self.status_feed();
        let author = feed.author(&author_jid)?;
        let at = self.status_pane.index_in(author.count());
        let message = feed.updates_of(author).nth(at)?;
        Some((message.id.clone(), message.media.clone()?))
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

    fn update(id: &str, read: bool) -> oxidezap_core::ChatMessage {
        let mut message =
            oxidezap_core::ChatMessage::new_outgoing(id.to_string(), "hi".to_string());
        message.is_from_me = false;
        message.is_read = read;
        message
    }

    /// Marking is against what is on screen; the *claim* is dropped against
    /// what the load itself carried. A row the merge kept — an older update
    /// past the page's end — still reads as watched here, which is exactly
    /// why the prune cannot be read off this.
    #[test]
    fn applying_a_view_marks_only_what_still_needs_it() {
        let mut chat = oxidezap_core::Chat::new(STATUS_BROADCAST_JID.to_string());
        chat.messages = vec![
            update("AGREED", true),
            update("OURS", false),
            update("THEIRS", false),
        ];
        let watched: std::collections::HashSet<String> = ["AGREED".to_string(), "OURS".to_string()]
            .into_iter()
            .collect();

        assert_eq!(apply_watched(&mut chat, &watched), 1);
        assert!(
            chat.messages[1].is_read,
            "the row the store had not caught up on"
        );
        assert!(
            !chat.messages[2].is_read,
            "an update nobody watched is left new"
        );
    }

    /// `is_read` on a row from us is the peer's read tick. Nothing local sets
    /// it, however the id got into the set.
    #[test]
    fn a_view_never_touches_one_of_our_own_updates() {
        let mut chat = oxidezap_core::Chat::new(STATUS_BROADCAST_JID.to_string());
        let mut ours = update("MINE", false);
        ours.is_from_me = true;
        chat.messages = vec![ours];
        let watched: std::collections::HashSet<String> = ["MINE".to_string()].into_iter().collect();

        assert_eq!(apply_watched(&mut chat, &watched), 0);
        assert!(!chat.messages[0].is_read, "the peer has still not read it");
    }

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
    fn a_reader_follows_its_update_rather_than_its_place() {
        let mut pane = StatusPane::default();
        pane.open("a@s.whatsapp.net".to_string());
        pane.step(true, 3);
        pane.follow(Some("second".to_string()));
        assert_eq!(pane.shown(), Some("second"));
        assert_eq!(pane.index_in(3), 1);

        // An older update arrives and is spliced in ahead of it: the same
        // message is now at 2, and that is where the reader belongs.
        pane.seek(2);
        assert_eq!(pane.index_in(4), 2);
        assert_eq!(pane.shown(), Some("second"), "still the same update");
    }

    #[test]
    fn opening_an_author_drops_the_previous_anchor() {
        let mut pane = StatusPane::default();
        pane.open("a@s.whatsapp.net".to_string());
        pane.follow(Some("first".to_string()));
        pane.open("b@s.whatsapp.net".to_string());
        assert_eq!(
            pane.shown(),
            None,
            "a new author's run has not resolved anything yet"
        );
        pane.follow(Some("theirs".to_string()));
        pane.close();
        assert_eq!(pane.shown(), None);
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
