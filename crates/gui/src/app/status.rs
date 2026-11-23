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

use gpui::{AppContext as _, Context, Entity, Window};
use gpui_component::input::{InputEvent, InputState};
use oxidezap_core::{
    ChatMessage, MediaType, QuotedKind, QuotedMessage, STATUS_BROADCAST_JID, StatusFeed,
};

use super::WhatsAppApp;

pub(super) struct StatusReply {
    feed_id: String,
    feed_author: String,
    quote: QuotedMessage,
    input: Entity<InputState>,
}

pub struct StatusPost {
    /// The text of a text status, or the caption over a photo or video.
    pub text: Entity<InputState>,
    pub search: Entity<InputState>,
    pub recipients: std::collections::BTreeSet<String>,
    pub pending: bool,
    /// A photo or video chosen to post instead of text.
    pub media: Option<StatusMediaDraft>,
}

/// A photo or video waiting to be posted as a status.
#[derive(Clone)]
pub struct StatusMediaDraft {
    pub file: crate::platform::picker::Picked,
    /// A still to show for an image; a video has none until it is posted.
    pub preview: Option<std::sync::Arc<gpui::Image>>,
}

/// Which of the sidebar's destinations is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Destination {
    #[default]
    Chats,
    Status,
    Channels,
    Communities,
}

impl Destination {
    pub const ALL: [Self; 4] = [Self::Chats, Self::Status, Self::Channels, Self::Communities];

    pub fn label(self) -> &'static str {
        match self {
            Self::Chats => crate::tr!(Chats),
            Self::Status => crate::tr!(Status),
            Self::Channels => crate::tr!(Channels),
            Self::Communities => crate::tr!(Communities),
        }
    }

    pub fn has_conversations(self) -> bool {
        self != Self::Status
    }

    pub fn matches(self, chat: &oxidezap_core::Chat) -> bool {
        use oxidezap_core::GroupHierarchy;
        if chat.is_status {
            return self == Self::Status;
        }
        let channel = chat.jid.ends_with("@newsletter");
        match self {
            Self::Channels => channel,
            Self::Communities => {
                !channel
                    && matches!(
                        chat.group_hierarchy,
                        Some(GroupHierarchy::Community | GroupHierarchy::Subgroup { .. })
                    )
            }
            // Community child groups are conversations too, as in WhatsApp Web.
            Self::Chats => {
                !channel && !matches!(chat.group_hierarchy, Some(GroupHierarchy::Community))
            }
            Self::Status => false,
        }
    }

    /// Stable per-destination element id, so a rebuilt rail keeps its buttons.
    pub fn id(self) -> &'static str {
        match self {
            Self::Chats => "nav-chats",
            Self::Status => "nav-status",
            Self::Channels => "nav-channels",
            Self::Communities => "nav-communities",
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
    playback_revision: u64,
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
        self.playback_revision = self.playback_revision.wrapping_add(1);
        self.shown = message_id;
    }

    pub fn playback_revision(&self) -> u64 {
        self.playback_revision
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
    pub fn set_destination(
        &mut self,
        destination: Destination,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if self.destination == destination {
            return;
        }
        // Switching the destination hides the conversation surface. Keep any
        // media request alive for caching, but revoke autoplay so a late
        // download cannot start sound behind Status or the chat list.
        self.cancel_hidden_media_autoplay();
        // Leaving Status is leaving the update that was playing. Every other
        // way out of the reader stops its media; this one changed which panel
        // was drawn and left a video decoding and talking underneath the
        // conversation that replaced it.
        if self.destination == Destination::Status {
            self.cancel_status_post(cx);
            self.leave_shown_status();
        }
        if destination == Destination::Status {
            self.refresh_status_mutes(cx);
        }
        if self.destination.has_conversations() {
            self.destination_selections
                .insert(self.destination.id(), self.selected_destination_jid());
        }
        self.destination_selections
            .entry(destination.id())
            .or_insert(None);
        self.destination = destination;
        // The status feed has no conversation view to request a timeline.
        // Load its first bounded page when it becomes visible, including when
        // an oversized initial snapshot reduced the history budget.
        if destination == Destination::Status {
            self.ensure_timeline_page(STATUS_BROADCAST_JID, cx);
        }
        self.search
            .update(cx, |search, cx| search.clear_list_query(window, cx));
        self.chat_list_scroll
            .scroll_to_item(0, gpui::ScrollStrategy::Top);
        self.invalidate_chat_cache();
        if let Some(jid) = self
            .destination_selections
            .get(destination.id())
            .cloned()
            .flatten()
            && self
                .chat_named(&jid)
                .is_some_and(|chat| destination.matches(chat))
        {
            self.select_chat(jid, super::ChatOpen::Restore, window, cx);
        }
        self.mobile_panel = crate::responsive::MobilePanel::ChatList;
        cx.notify();
    }

    /// The broadcast, grouped by author.
    ///
    /// Cached against the message count and dropped by the same invalidation
    /// that drops the chat list's: one pass over the updates per change, not
    /// one per frame.
    /// The feed without anyone whose status updates the account has muted.
    /// Applied after the cache, because a mute changes without a message
    /// arriving.
    pub fn status_feed(&self) -> StatusFeed {
        let mut feed = self.unfiltered_status_feed();
        feed.retain_authors(|author| !self.status_muted.contains(&author.jid));
        feed
    }

    /// Ask the daemon whose status updates are muted. Asked each time Status is
    /// opened, and only then: the first answer is a snapshot of the collection
    /// the phone keeps chat mutes and pins in, which is not worth fetching for
    /// a window that never looks at Status. Free once the daemon has it.
    pub fn refresh_status_mutes(&mut self, cx: &mut Context<Self>) {
        let Some(client) = &self.client else {
            return;
        };
        let answer = client.status_mutes();
        cx.spawn(async move |app, cx| {
            match answer.await {
                Ok(Ok(jids)) => {
                    let _ = app.update(cx, |app, cx| {
                        app.status_muted = jids.into_iter().collect();
                        cx.notify();
                    });
                }
                Ok(Err(failure)) => log::debug!("status mutes unavailable: {failure}"),
                Err(_) => {}
            };
        })
        .detach();
    }

    fn unfiltered_status_feed(&self) -> StatusFeed {
        let chat = self.chats.iter().find(|chat| chat.is_status);
        let count = chat.map_or(0, |chat| chat.messages.len());

        let now = oxidezap_core::time::now_utc();
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
            .map(|chat| {
                StatusFeed::from_chat_with_names(chat, now, |jid| {
                    self.chat_named(jid).map(|chat| chat.name.clone())
                })
            })
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
        let wait = (when - oxidezap_core::time::now_utc())
            .to_std()
            .unwrap_or(std::time::Duration::ZERO)
            .max(std::time::Duration::from_secs(1));

        self.status_tick_at = Some(when);
        self.status_tick = Some(cx.spawn(async move |entity: gpui::WeakEntity<Self>, cx| {
            crate::platform::sleep(wait).await;
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

    pub fn status_reply_input(
        &mut self,
        author: &str,
        author_name: &str,
        message: &ChatMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<InputState>> {
        if author.is_empty() || message.is_from_me {
            self.status_reply = None;
            return None;
        }
        if let Some(reply) = &self.status_reply
            && reply.feed_id == message.id
            && reply.feed_author == author
        {
            return Some(reply.input.clone());
        }

        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(crate::tr!(ReplyPrivately)));
        cx.subscribe_in(
            &input,
            window,
            |app, source, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    app.send_status_reply(source, window, cx);
                }
            },
        )
        .detach();

        let key = oxidezap_core::StatusPostKey::wire_key(&message.id, author, message.is_from_me);
        self.status_reply = Some(StatusReply {
            feed_id: message.id.clone(),
            feed_author: author.to_owned(),
            quote: QuotedMessage {
                remote_jid: Some(key.origin),
                message_id: key.id,
                sender: key.sender,
                sender_name: message.sender_name.clone().unwrap_or_else(|| {
                    if message.sender == author {
                        author_name.to_owned()
                    } else {
                        oxidezap_core::fallback_sender_name(&message.sender).into_owned()
                    }
                }),
                preview: message.content.clone(),
                kind: message
                    .media
                    .as_ref()
                    .and_then(|media| QuotedKind::of(&media.media_type)),
            },
            input: input.clone(),
        });
        Some(input)
    }

    pub fn send_status_reply(
        &mut self,
        input: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(reply) = &self.status_reply else {
            return;
        };
        if self.destination != Destination::Status
            || &reply.input != input
            || self.status_pane.author() != Some(reply.feed_author.as_str())
            || self.shown_status_message_id().as_deref() != Some(reply.feed_id.as_str())
        {
            return;
        }
        let value = input.read(cx).value();
        let text = value.trim();
        if text.is_empty() {
            return;
        }
        let quote = reply.quote.clone();
        let jid = quote.sender.clone();
        if self.send_text_to(&jid, text, Some(quote), cx) {
            self.status_reply = None;
            self.select_chat(jid, super::ChatOpen::ToPreview, window, cx);
            self.scroll_to_last_message();
        }
    }

    pub fn status_post(&self) -> Option<&StatusPost> {
        self.status_post.as_ref()
    }

    pub fn status_recipients(&self, query: &str) -> impl Iterator<Item = (&str, &str)> {
        let query = query.trim().to_lowercase();
        self.chats.iter().filter_map(move |chat| {
            let (number, server) = chat.jid.split_once('@')?;
            if !matches!(server, "s.whatsapp.net" | "lid")
                || self.account_jid.as_deref() == Some(chat.jid.as_str())
                || self.account_lid.as_deref() == Some(chat.jid.as_str())
            {
                return None;
            }
            (query.is_empty()
                || chat.name.to_lowercase().contains(&query)
                || number.contains(&query))
            .then_some((chat.jid.as_str(), chat.name.as_str()))
        })
    }

    pub fn begin_status_post(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_send() || self.status_post.is_some() {
            return;
        }
        self.close_status(cx);
        let text = cx.new(|cx| InputState::new(window, cx).placeholder(crate::tr!(WriteAStatus)));
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(crate::tr!(SearchChats)));
        for field in [&text, &search] {
            cx.subscribe(field, |_, _, _: &InputEvent, cx| cx.notify())
                .detach();
        }
        text.update(cx, |input, cx| input.focus(window, cx));
        self.status_post = Some(StatusPost {
            text,
            search,
            recipients: Default::default(),
            pending: false,
            media: None,
        });
        self.navigate_to_chat();
        cx.notify();
    }

    /// Ask for a photo or video to post. Only images and videos qualify; a
    /// document, a sticker or a voice note is not a status.
    pub fn choose_status_media(&mut self, cx: &mut Context<Self>) {
        if !self.can_send() || self.status_post.as_ref().is_none_or(|post| post.pending) {
            return;
        }
        // Which draft asked: the chooser and the read take as long as they
        // take, and a draft cancelled and replaced meanwhile must not be
        // handed the old one's file.
        let origin = self.status_post.as_ref().map(|post| post.text.downgrade());
        let chosen = crate::platform::picker::choose_category(
            cx,
            crate::platform::picker::AttachmentCategory::PhotosVideos,
        );
        cx.spawn(async move |app, cx| {
            let chosen = chosen.await;
            let _ = app.update(cx, |app, cx| {
                let Some(post) = &mut app.status_post else {
                    return;
                };
                if post.pending
                    || origin.as_ref().and_then(|origin| origin.upgrade()).as_ref()
                        != Some(&post.text)
                {
                    return;
                }
                match chosen {
                    Ok(chosen) => {
                        for refusal in chosen.refused {
                            app.notify_user(refusal, super::notices::Tone::Problem, cx);
                        }
                        let Some(post) = &mut app.status_post else {
                            return;
                        };
                        if let Some(file) = chosen.files.into_iter().find(|file| {
                            matches!(
                                file.kind,
                                oxidezap_core::OutgoingMedia::Image
                                    | oxidezap_core::OutgoingMedia::Video
                            )
                        }) {
                            // A still is a second copy of the picture, so only
                            // a modest one is drawn; the file itself is sent
                            // either way.
                            const PREVIEW_LIMIT: usize = 16 * 1024 * 1024;
                            let preview = (file.kind == oxidezap_core::OutgoingMedia::Image
                                && file.bytes.len() <= PREVIEW_LIMIT)
                                .then(|| crate::utils::mime_to_image_format(&file.mime_type))
                                .flatten()
                                .map(|format| {
                                    std::sync::Arc::new(gpui::Image::from_bytes(
                                        format,
                                        file.bytes.clone(),
                                    ))
                                });
                            post.media = Some(StatusMediaDraft { file, preview });
                        }
                    }
                    Err(error) => {
                        let _ = post;
                        app.notify_user(error, super::notices::Tone::Problem, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn clear_status_media(&mut self, cx: &mut Context<Self>) {
        if let Some(post) = &mut self.status_post
            && !post.pending
        {
            post.media = None;
            cx.notify();
        }
    }

    pub fn cancel_status_post(&mut self, cx: &mut Context<Self>) {
        if self.status_post.as_ref().is_some_and(|post| !post.pending) {
            self.status_post = None;
            if !self.status_pane.is_open() {
                self.mobile_panel = crate::responsive::MobilePanel::ChatList;
            }
            cx.notify();
        }
    }

    pub fn set_status_recipient(&mut self, jid: &str, selected: bool, cx: &mut Context<Self>) {
        let Some(post) = &mut self.status_post else {
            return;
        };
        if post.pending {
            return;
        }
        if selected {
            post.recipients.insert(jid.to_owned());
        } else {
            post.recipients.remove(jid);
        }
        cx.notify();
    }

    pub fn publish_status(&mut self, cx: &mut Context<Self>) {
        if !self.can_send() {
            return;
        }
        let Some(client) = &self.client else {
            return;
        };
        let Some(post) = &mut self.status_post else {
            return;
        };
        let text = post.text.read(cx).value();
        let text = text.trim();
        if post.pending || (text.is_empty() && post.media.is_none()) {
            return;
        }
        post.pending = true;
        let draft = post.text.downgrade();
        let recipients = post.recipients.iter().cloned().collect();
        let result = match &post.media {
            Some(media) => {
                let crate::platform::picker::Picked {
                    file_name,
                    mime_type,
                    kind,
                    bytes,
                } = media.file.clone();
                client.publish_status_media(
                    crate::session::Attachment {
                        bytes,
                        kind,
                        mime_type,
                        file_name,
                        caption: (!text.is_empty()).then(|| text.to_owned()),
                    },
                    recipients,
                )
            }
            None => client.publish_status(text.to_owned(), recipients),
        };
        cx.spawn(async move |app, cx| {
            let result = result
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = app.update(cx, |app, cx| {
                let Some(post) = &mut app.status_post else {
                    return;
                };
                if draft.upgrade().as_ref() != Some(&post.text) {
                    return;
                }
                post.pending = false;
                match result {
                    Ok(()) => {
                        // Not `cancel_status_post`: it also puts a phone's
                        // layout back on the chat list, which would take the
                        // person out of whatever they opened while this was
                        // posting.
                        app.status_post = None;
                        app.notify_user(
                            crate::tr!(StatusPublished),
                            super::notices::Tone::Info,
                            cx,
                        );
                    }
                    Err(error) => app.notify_user(error, super::notices::Tone::Problem, cx),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub fn open_quoted_status(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let feed = self.status_feed();
        let target = feed
            .mine()
            .into_iter()
            .chain(feed.authors())
            .find_map(|author| {
                let index = feed
                    .updates_of(author)
                    .position(|message| message.id == id)?;
                Some((author.jid.clone(), index))
            });
        let Some((author, index)) = target else {
            self.notify_user(
                crate::tr!(OriginalStatusUnavailable),
                super::notices::Tone::Info,
                cx,
            );
            return;
        };
        self.set_destination(Destination::Status, window, cx);
        let leaving = self.shown_status_message_id();
        self.status_pane.open(author);
        self.status_pane.seek(index);
        self.navigate_to_chat();
        self.shown_status_changed(leaving, cx);
        cx.notify();
    }

    pub fn open_status(&mut self, author: String, cx: &mut Context<Self>) {
        // Read before the selection moves, because afterwards there is no way
        // to ask what was on screen. Opening an image or a text update does
        // not touch the player, so a video left this way went on playing.
        // While a post is in flight its composer stays on screen, so a
        // reader opened now would play behind it. Navigation resumes when the
        // post settles.
        if self.status_post.as_ref().is_some_and(|post| post.pending) {
            return;
        }
        self.cancel_status_post(cx);
        let leaving = self.shown_status_message_id();
        self.status_reply = None;
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
        self.status_reply = None;
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
        let Some(author_jid) = self.status_pane.author().map(str::to_string) else {
            return;
        };
        // Asked after the author, and allowed to be absent. `follow` records
        // `None` when the author being opened had nothing in the feed, which
        // a short race between the row being drawn and the run lapsing can
        // returning on that left the reader open on an empty pane forever,
        // because the branch that closes it is below.
        let anchor = self.status_pane.shown().map(str::to_string);
        let feed = self.status_feed();
        let Some(author) = feed.author(&author_jid) else {
            // Their whole run is gone — the last of it lapsed, or the only
            // update they had was taken back. Same as an anchor that is no
            // longer in the run, except that there is nothing behind it
            // either: stop what was playing and close the reader, which would
            // otherwise be an empty state with no way out of it on a phone.
            self.status_pane.follow(None);
            self.stop_status_media(anchor);
            self.close_status(cx);
            return;
        };
        // There is a run, and the reader is following nothing in it. Adopt
        // whatever its position resolves to, announced like any other change:
        // nothing is being left, so there is nothing to stop.
        let Some(anchor) = anchor else {
            self.shown_status_changed(None, cx);
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
        self.status_reply = None;
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
            self.stop_hidden_media();
        } else if let Some(player) = self.video_players.get_mut(&id) {
            player.stop_hidden();
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

        let Some(chat) = self
            .chats
            .iter_mut()
            .find(|chat| chat.is_status)
            .map(std::sync::Arc::make_mut)
        else {
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
        let Some(chat) = self
            .chats
            .iter_mut()
            .find(|chat| chat.is_status)
            .map(std::sync::Arc::make_mut)
        else {
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
        if self.shown_status_awaits_cache() {
            // The render pass will request the one update it actually draws.
            return;
        }
        self.fetch_shown_status_after_cache(cx);
    }

    pub(super) fn shown_status_awaits_cache(&self) -> bool {
        let Some(id) = self.shown_status_message_id() else {
            return false;
        };
        self.chat_named(STATUS_BROADCAST_JID)
            .and_then(|chat| chat.messages.iter().find(|message| message.id == id))
            .is_some_and(|message| self.awaiting_cached_media(STATUS_BROADCAST_JID, message))
    }

    pub(super) fn fetch_shown_status_after_cache(&mut self, cx: &mut Context<Self>) {
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
            MediaType::Image if needs_bytes => {
                self.download_image(STATUS_BROADCAST_JID.into(), message_id, downloadable, cx)
            }
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

    #[test]
    fn destinations_separate_channels_and_community_containers_but_keep_child_conversations() {
        use oxidezap_core::{Chat, GroupHierarchy, SubgroupKind};
        let direct = Chat::new("peer@example.invalid".into());
        let channel = Chat::new("channel@newsletter".into());
        let mut parent = Chat::new("community@g.us".into());
        parent.is_group = true;
        parent.group_hierarchy = Some(GroupHierarchy::Community);
        let mut child = Chat::new("child@g.us".into());
        child.is_group = true;
        child.group_hierarchy = Some(GroupHierarchy::Subgroup {
            parent_jid: parent.jid.clone(),
            kind: SubgroupKind::Announcement,
        });
        let status = Chat::new(STATUS_BROADCAST_JID.into());
        for (chat, expected) in [
            (&direct, [true, false, false, false]),
            (&channel, [false, false, true, false]),
            (&parent, [false, false, false, true]),
            (&child, [true, false, false, true]),
            (&status, [false, true, false, false]),
        ] {
            assert_eq!(
                Destination::ALL.map(|destination| destination.matches(chat)),
                expected,
                "{}",
                chat.jid
            );
        }
        parent.group_hierarchy = None;
        assert!(
            Destination::Chats.matches(&parent),
            "unknown relationships stay reachable"
        );
        assert!(!Destination::Communities.matches(&parent));
    }

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
