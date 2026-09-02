//! The fullscreen media viewer.
//!
//! A photo in a bubble is a thumbnail of a photo, and the bubble is the wrong
//! size to look at one. The viewer is the same picture at the size of the
//! window, and it walks the conversation's other pictures, because that is
//! what someone who opened one is usually doing.

use gpui::{Context, FocusHandle};
use oxidezap_core::{ChatMessage, MediaType};

/// The picture on screen full size, and the keyboard it takes while it is.
///
/// An entity because a viewer is a *mode*: while one is up it owns the arrow
/// keys, the conversation behind it is not being read, and every path that
/// changes a chat has to ask whether what it is showing still exists. Those
/// are decisions about one thing, and the context its methods take is what
/// keeps them from being made about everything else at the same time.
///
/// It holds no messages. Every method that has to look at one is handed the
/// conversation's messages by the window, which is also what stops the
/// alternative: cloning a whole message vector to hand to a viewer that owns
/// three strings.
pub(super) struct Viewer {
    /// The picture being looked at full screen, when one is.
    showing: Option<MediaViewer>,
    /// Focus target, which owns the arrow keys only while the viewer is up.
    ///
    /// Made once and kept: a handle rebuilt per opening is a handle the
    /// frame's focus is no longer on, and a transient surface that takes the
    /// keyboard has to be able to give it back.
    focus: FocusHandle,
}

impl Viewer {
    pub(super) fn new(cx: &mut Context<Self>) -> Self {
        Self {
            showing: None,
            focus: cx.focus_handle(),
        }
    }

    pub(super) fn focus(&self) -> &FocusHandle {
        &self.focus
    }

    pub(super) fn showing(&self) -> Option<&MediaViewer> {
        self.showing.as_ref()
    }

    /// Which conversation's pictures are being walked, if any.
    pub(super) fn jid(&self) -> Option<&str> {
        self.showing.as_ref().map(|viewer| viewer.jid.as_str())
    }

    /// The id of the picture on screen, which may not be evicted from the
    /// decoded cache while it is.
    pub(super) fn current_id(&self) -> Option<&str> {
        self.showing.as_ref().and_then(MediaViewer::current_id)
    }

    pub(super) fn open(&mut self, viewer: MediaViewer, cx: &mut Context<Self>) {
        self.showing = Some(viewer);
        cx.notify();
    }

    pub(super) fn close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.showing.take().is_some() {
            cx.notify();
            return true;
        }
        false
    }

    /// Walk to the next picture in the conversation.
    ///
    /// Re-resolved first: a download finishing adds a picture either side,
    /// and stepping over a stale list would skip it.
    pub(super) fn step(
        &mut self,
        forward: bool,
        messages: Option<&[ChatMessage]>,
        cx: &mut Context<Self>,
    ) {
        if self.reconcile(messages, cx)
            && let Some(viewer) = &mut self.showing
        {
            viewer.step(forward);
            cx.notify();
        }
    }

    /// Point the viewer at what its chat holds now, closing it when there is
    /// nothing left to look at. Says whether one is still open.
    ///
    /// The viewer names the picture it is showing and resolves it on every
    /// frame, so a revoke behind it left a modal that drew nothing and still
    /// swallowed the Escape meant to close it — a window that had stopped
    /// responding, as far as anyone looking at it could tell.
    ///
    /// `None` for the messages is the conversation itself being gone, and so
    /// is everything it held.
    pub(super) fn reconcile(
        &mut self,
        messages: Option<&[ChatMessage]>,
        cx: &mut Context<Self>,
    ) -> bool {
        let survives = Self::resolve(&mut self.showing, messages);
        cx.notify();
        survives
    }

    /// The half of [`Self::reconcile`] with no window in it: point the
    /// viewer at what its chat holds now, and close it when there is nothing
    /// left to look at.
    /// Takes the slot rather than `self`, which is what lets a test drive it:
    /// a [`Viewer`] owns a focus handle, and a focus handle needs a window.
    fn resolve(showing: &mut Option<MediaViewer>, messages: Option<&[ChatMessage]>) -> bool {
        let Some(viewer) = showing else {
            return false;
        };
        if viewer.refresh(messages.unwrap_or_default()) {
            return true;
        }
        *showing = None;
        false
    }

    /// Close a viewer that is not about `jid`.
    ///
    /// One left open over a chat that is no longer on screen draws nothing,
    /// keeps the keyboard, and swallows the Escape meant to close it.
    pub(super) fn close_unless_about(&mut self, jid: &str, cx: &mut Context<Self>) {
        if self
            .showing
            .as_ref()
            .is_some_and(|viewer| viewer.jid != jid)
        {
            self.showing = None;
            cx.notify();
        }
    }

    /// Close a viewer whose conversation has gone.
    pub(super) fn close_if_about_any(&mut self, jids: &[String], cx: &mut Context<Self>) {
        if self
            .showing
            .as_ref()
            .is_some_and(|viewer| jids.contains(&viewer.jid))
        {
            self.showing = None;
            cx.notify();
        }
    }
}

/// What the viewer is showing.
pub struct MediaViewer {
    /// The conversation it was opened from. Leaving that conversation closes
    /// it: the pictures it walks are that chat's.
    pub jid: String,
    /// Every viewable message in the chat, oldest first — the order the
    /// timeline puts them in, so left and right mean what they look like.
    pub items: Vec<String>,
    pub current: usize,
}

impl MediaViewer {
    /// Open at `message_id`, with the chat's other pictures either side.
    ///
    /// `None` when the message is not something to look at — a voice note or
    /// a document has nothing to show full screen.
    pub fn open(jid: String, message_id: &str, messages: &[ChatMessage]) -> Option<Self> {
        let items: Vec<String> = messages
            .iter()
            .filter(|message| is_viewable(message))
            .map(|message| message.id.clone())
            .collect();
        let current = items.iter().position(|id| id == message_id)?;
        Some(Self {
            jid,
            items,
            current,
        })
    }

    pub fn current_id(&self) -> Option<&str> {
        self.items.get(self.current).map(String::as_str)
    }

    /// "3 of 12", or nothing when there is only the one.
    pub fn position(&self) -> Option<String> {
        (self.items.len() > 1).then(|| format!("{} of {}", self.current + 1, self.items.len()))
    }

    pub fn can_step(&self) -> bool {
        self.items.len() > 1
    }

    /// Step to the next picture, wrapping.
    pub fn step(&mut self, forward: bool) {
        if self.items.len() < 2 {
            return;
        }
        let last = self.items.len() - 1;
        self.current = if forward {
            if self.current >= last {
                0
            } else {
                self.current + 1
            }
        } else if self.current == 0 {
            last
        } else {
            self.current - 1
        };
    }

    /// Re-resolve against the chat's current messages.
    ///
    /// A download completing rewrites the row, and a message can be revoked
    /// while the viewer is open. Returns whether the viewer still has
    /// anything to show.
    pub fn refresh(&mut self, messages: &[ChatMessage]) -> bool {
        let showing = self.current_id().map(str::to_string);
        self.items = messages
            .iter()
            .filter(|message| is_viewable(message))
            .map(|message| message.id.clone())
            .collect();
        if self.items.is_empty() {
            return false;
        }
        self.current = showing
            .and_then(|id| self.items.iter().position(|candidate| *candidate == id))
            // The one being looked at is gone; the nearest surviving
            // neighbour is a better answer than closing on the reader.
            .unwrap_or_else(|| self.current.min(self.items.len() - 1));
        true
    }
}

/// Whether a message is something the viewer can show.
///
/// Downloaded bytes are the bar, not the media kind: an image whose bytes are
/// still a blurred placeholder would open to a blurred placeholder at full
/// size, which reads as a broken viewer rather than a pending download.
///
/// Pictures only. A video's bytes are an encoded stream, and the viewer has
/// no decoder of its own — opening one would show the "cannot be shown"
/// placeholder for something it had just called viewable. Video plays in its
/// bubble until the viewer grows a player.
fn is_viewable(message: &ChatMessage) -> bool {
    message.media.as_ref().is_some_and(|media| {
        media.media_type == MediaType::Image && !media.data.is_empty() && !media.data_is_preview
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_core::MediaContent;

    fn message(id: &str, media: Option<MediaContent>) -> ChatMessage {
        let mut message = ChatMessage::new_incoming(
            id.to_string(),
            "a@s.whatsapp.net".to_string(),
            String::new(),
        );
        message.media = media;
        message
    }

    fn image(preview: bool, downloaded: bool) -> MediaContent {
        MediaContent::image(
            std::sync::Arc::new(if downloaded {
                vec![1, 2, 3]
            } else {
                Vec::new()
            }),
            "image/jpeg".to_string(),
            preview,
        )
        .with_size(Some(100), Some(100))
    }

    /// Deliberately not `MediaContent::video`, which would call its bytes a
    /// poster and so a preview: what the gate above has to be shown refusing
    /// is the *kind*, on media that passes every other test it applies.
    fn video() -> MediaContent {
        MediaContent {
            media_type: MediaType::Video,
            ..image(false, true)
        }
    }

    fn history() -> Vec<ChatMessage> {
        vec![
            message("text", None),
            message("photo-1", Some(image(false, true))),
            message("thumb", Some(image(true, true))),
            message("clip", Some(video())),
            message("photo-2", Some(image(false, true))),
        ]
    }

    /// The conversation itself is gone — a complete load said so, or the
    /// account was replaced — and so is everything it held. A viewer left
    /// open over it draws nothing, keeps the keyboard, and swallows the
    /// Escape meant to close it.
    #[test]
    fn a_viewer_whose_conversation_has_gone_closes() {
        let mut showing = MediaViewer::open("chat".into(), "photo-1", &history());
        assert!(showing.is_some());

        assert!(!Viewer::resolve(&mut showing, None));
        assert!(showing.is_none());
    }

    /// And where the chat is still there but has nothing left to look at,
    /// the viewer goes: a modal showing no picture is one nobody can read.
    #[test]
    fn a_conversation_with_no_pictures_left_closes_it() {
        let mut showing = MediaViewer::open("chat".into(), "photo-1", &history());

        assert!(!Viewer::resolve(
            &mut showing,
            Some(&[message("text", None)])
        ));
        assert!(showing.is_none());
    }

    #[test]
    fn a_video_is_not_something_the_viewer_can_show() {
        assert!(MediaViewer::open("chat".into(), "clip", &history()).is_none());
        let viewer = MediaViewer::open("chat".into(), "photo-1", &history()).expect("viewable");
        assert!(
            !viewer.items.iter().any(|id| id == "clip"),
            "and stepping does not land on one"
        );
    }

    #[test]
    fn the_viewer_walks_only_what_it_can_show() {
        let viewer = MediaViewer::open("chat".into(), "photo-1", &history()).expect("viewable");
        assert_eq!(viewer.items, vec!["photo-1".to_string(), "photo-2".into()]);
        assert_eq!(viewer.position().as_deref(), Some("1 of 2"));
    }

    #[test]
    fn a_thumbnail_is_not_something_to_open() {
        assert!(MediaViewer::open("chat".into(), "thumb", &history()).is_none());
        assert!(MediaViewer::open("chat".into(), "text", &history()).is_none());
    }

    #[test]
    fn stepping_wraps_in_both_directions() {
        let mut viewer = MediaViewer::open("chat".into(), "photo-2", &history()).unwrap();
        viewer.step(true);
        assert_eq!(viewer.current_id(), Some("photo-1"));
        viewer.step(false);
        assert_eq!(viewer.current_id(), Some("photo-2"));
    }

    #[test]
    fn one_picture_has_nowhere_to_step() {
        let messages = vec![message("photo-1", Some(image(false, true)))];
        let mut viewer = MediaViewer::open("chat".into(), "photo-1", &messages).unwrap();
        assert!(!viewer.can_step());
        assert!(viewer.position().is_none());
        viewer.step(true);
        assert_eq!(viewer.current_id(), Some("photo-1"));
    }

    #[test]
    fn losing_the_open_picture_falls_back_to_a_neighbour() {
        let mut viewer = MediaViewer::open("chat".into(), "photo-2", &history()).unwrap();
        let remaining = vec![message("photo-1", Some(image(false, true)))];
        assert!(viewer.refresh(&remaining));
        assert_eq!(viewer.current_id(), Some("photo-1"));
    }

    #[test]
    fn losing_every_picture_closes_the_viewer() {
        let mut viewer = MediaViewer::open("chat".into(), "photo-1", &history()).unwrap();
        assert!(!viewer.refresh(&[message("text", None)]));
    }
}
