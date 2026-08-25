//! The message a reply points back at.

use serde::{Deserialize, Serialize};

/// A one-line summary of the message being replied to.
///
/// Deliberately a snapshot rather than a reference: the original may be
/// outside the loaded window, deleted, or in a chat the store has since
/// pruned, and a reply must still render its quote. [`Self::message_id`] is
/// what lets the UI jump to the original *when* it is present, and the
/// snapshot is what it falls back to when it is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotedMessage {
    /// Id of the original, for the jump-to-original gesture.
    pub message_id: String,
    /// JID of whoever wrote it, which is what colours the quote bar — the
    /// same hue that person has everywhere else.
    pub sender: String,
    /// Display name at the time of quoting.
    pub sender_name: String,
    /// One line of the original. Empty for a media-only message, where
    /// [`Self::kind`] is what there is to say.
    pub preview: String,
    /// What the original was, when it was not text.
    pub kind: Option<QuotedKind>,
}

/// The non-text shape of a quoted message, so the quote can say `Photo`
/// rather than nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotedKind {
    Image,
    Video,
    Audio,
    Document,
    Sticker,
}

impl QuotedKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Image => "Photo",
            Self::Video => "Video",
            Self::Audio => "Voice message",
            Self::Document => "Document",
            Self::Sticker => "Sticker",
        }
    }
}

impl QuotedMessage {
    /// The line to draw under the sender's name.
    ///
    /// A caption wins over the media label: it is what the person actually
    /// wrote, and repeating `Photo` when there is a caption tells the reader
    /// less than the caption does.
    pub fn summary(&self) -> &str {
        if !self.preview.is_empty() {
            &self.preview
        } else {
            self.kind.map(QuotedKind::label).unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quoted(preview: &str, kind: Option<QuotedKind>) -> QuotedMessage {
        QuotedMessage {
            message_id: "ABC".to_string(),
            sender: "a@s.whatsapp.net".to_string(),
            sender_name: "Ana".to_string(),
            preview: preview.to_string(),
            kind,
        }
    }

    #[test]
    fn text_is_quoted_verbatim() {
        assert_eq!(
            quoted("e o áudio, tá saindo?", None).summary(),
            "e o áudio, tá saindo?"
        );
    }

    #[test]
    fn media_without_a_caption_names_its_kind() {
        assert_eq!(quoted("", Some(QuotedKind::Image)).summary(), "Photo");
        assert_eq!(
            quoted("", Some(QuotedKind::Audio)).summary(),
            "Voice message"
        );
    }

    #[test]
    fn a_caption_wins_over_the_media_label() {
        assert_eq!(
            quoted("no sítio", Some(QuotedKind::Image)).summary(),
            "no sítio"
        );
    }

    #[test]
    fn an_unknown_original_summarises_to_nothing_rather_than_panicking() {
        assert_eq!(quoted("", None).summary(), "");
    }
}
