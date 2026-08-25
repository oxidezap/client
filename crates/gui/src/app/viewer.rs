//! The fullscreen media viewer.
//!
//! A photo in a bubble is a thumbnail of a photo, and the bubble is the wrong
//! size to look at one. The viewer is the same picture at the size of the
//! window, and it walks the conversation's other pictures, because that is
//! what someone who opened one is usually doing.

use oxidezap_core::{ChatMessage, MediaType};

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
        MediaContent {
            media_type: MediaType::Image,
            data: std::sync::Arc::new(if downloaded {
                vec![1, 2, 3]
            } else {
                Vec::new()
            }),
            cache_key: None,
            mime_type: "image/jpeg".to_string(),
            width: Some(100),
            height: Some(100),
            caption: None,
            file_name: None,
            downloadable: None,
            is_animated: false,
            duration_secs: None,
            data_is_preview: preview,
            waveform: None,
        }
    }

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
