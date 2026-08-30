//! What a conversation row shows, worked out once per frame.
//!
//! Everything a row needs is derived here so the renderer stays declarative
//! and the decisions — which preview wins, whose tick to draw, whether the
//! badge is a count or a dot — can be tested without a window.

use chrono::{DateTime, Utc};
use oxidezap_core::{
    Chat, ChatMessage, MediaType, MessageStatus, SystemNotice, TypingSummary, format_duration,
    plain_message_text,
};

/// The badge at the end of a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unread {
    None,
    /// How many messages arrived unread.
    Count(u32),
    /// Marked unread by hand. WhatsApp's `-1` sentinel: a badge with no
    /// number, so it is drawn as a dot rather than a pill containing a bullet.
    Marked,
}

/// The glyph in front of a media preview, so `Photo` is recognisable before
/// the word is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewGlyph {
    Image,
    Video,
    Audio,
    Document,
    Sticker,
}

impl PreviewGlyph {
    pub(crate) fn of(media_type: &MediaType) -> Self {
        match media_type {
            MediaType::Image => Self::Image,
            MediaType::Video => Self::Video,
            MediaType::Audio => Self::Audio,
            MediaType::Document => Self::Document,
            MediaType::Sticker => Self::Sticker,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Image => "Photo",
            Self::Video => "Video",
            Self::Audio => "Voice message",
            Self::Document => "Document",
            Self::Sticker => "Sticker",
        }
    }
}

/// The second line of a row.
#[derive(Debug, Clone, PartialEq)]
pub enum Preview {
    /// The conversation exists but has no messages yet.
    Empty,
    /// Somebody is typing. Outranks the last message: it is the newer fact,
    /// and it is the one that expires.
    Typing(TypingSummary),
    /// An unsent draft, which outranks the last message because it is what the
    /// user will want to come back to.
    Draft(String),
    Message {
        /// `Ana:` in a group, `You:` for our own last message.
        prefix: Option<String>,
        glyph: Option<PreviewGlyph>,
        text: String,
        /// The tick, when the last message is ours.
        status: Option<MessageStatus>,
    },
}

/// One row of the conversation list, ready to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRow {
    pub jid: String,
    pub name: String,
    pub is_group: bool,
    pub timestamp: Option<DateTime<Utc>>,
    pub unread: Unread,
    pub preview: Preview,
}

impl ChatRow {
    /// Build the row for `chat`.
    ///
    /// `typing` and `draft` are transient state the front end owns; the chat
    /// itself knows nothing about either.
    pub fn new(
        chat: &Chat,
        typing: Option<TypingSummary>,
        draft: Option<&str>,
        is_own_number: bool,
    ) -> Self {
        Self {
            jid: chat.jid.clone(),
            name: display_name(&chat.name, is_own_number),
            is_group: chat.is_group,
            timestamp: chat.last_message_time,
            unread: if chat.unread_count > 0 {
                Unread::Count(chat.unread_count)
            } else if chat.manually_unread {
                Unread::Marked
            } else {
                Unread::None
            },
            preview: preview_for(chat, typing, draft, is_own_number),
        }
    }

    /// Whether the row's time and badge should read as "there is something
    /// here for you".
    pub fn has_unread(&self) -> bool {
        !matches!(self.unread, Unread::None)
    }
}

fn preview_for(
    chat: &Chat,
    typing: Option<TypingSummary>,
    draft: Option<&str>,
    is_own_number: bool,
) -> Preview {
    // Ordered by which fact is most worth the one line available: someone
    // typing now, then something the user started writing, then history.
    if let Some(summary) = typing {
        return Preview::Typing(summary);
    }
    if let Some(draft) = draft.map(str::trim).filter(|d| !d.is_empty()) {
        return Preview::Draft(single_line(draft));
    }

    let Some(last) = chat.messages.last() else {
        // A chat with a stored preview string but no loaded messages — the
        // list hydrates before the timeline does.
        return match chat.last_message.as_deref() {
            Some(text) if !text.is_empty() => Preview::Message {
                prefix: None,
                glyph: None,
                text: single_line(text),
                status: None,
            },
            _ => Preview::Empty,
        };
    };

    Preview::Message {
        prefix: prefix_for(chat, last),
        glyph: last.media.as_ref().map(|m| PreviewGlyph::of(&m.media_type)),
        text: body_for(last),
        status: last.delivery_in(is_own_number),
    }
}

/// Who wrote the last message, when that is not obvious from the row itself.
fn prefix_for(chat: &Chat, last: &ChatMessage) -> Option<String> {
    // A call record and a group notice belong to the conversation rather than
    // to a side of it, so neither takes a "You:" or a sender name.
    if last.system.is_some() {
        return None;
    }
    if last.is_from_me {
        // The tick already says it is ours in a 1:1 chat; in a group the name
        // column is the group's, so the sender still needs saying.
        return chat.is_group.then(|| "You".to_string());
    }
    if !chat.is_group {
        return None;
    }
    chat.author_name(last)
        .map(|name| crate::utils::capped_name(&single_line(name)))
}

/// The preview text: a caption if there is one, otherwise the media's name.
fn body_for(last: &ChatMessage) -> String {
    // A row nobody typed still happened, and a call is exactly the thing a
    // reader scans the list for. Its own sentence rather than the empty
    // string a message with no content would otherwise leave behind.
    if let Some(notice) = &last.system {
        return match notice {
            SystemNotice::Call(record) => record.summary(),
            SystemNotice::GroupChanged(text) => single_line(text),
        };
    }
    if !last.content.is_empty() {
        // The markers come out here too. A preview is one unstyled line with
        // nowhere to put emphasis, and the bubble beside it already renders
        // the effect — a row still showing `*bold*` is the only place the
        // markup leaks.
        return single_line(&plain_message_text(&last.content));
    }
    match last.media.as_ref() {
        Some(media) => {
            let label = PreviewGlyph::of(&media.media_type).label();
            // A voice note's length is the useful part of its preview — it is
            // the difference between "worth playing now" and "later".
            match media
                .duration_secs
                .filter(|_| matches!(media.media_type, MediaType::Audio | MediaType::Video))
            {
                Some(secs) => format!("{label} · {}", format_duration(secs)),
                None => label.to_string(),
            }
        }
        None => String::new(),
    }
}

/// The name as the list shows it.
///
/// WhatsApp marks the conversation with your own number "(You)", and it is
/// the one row where the name alone is ambiguous: a second number of yours is
/// saved under a person's name like any other contact, so without the suffix
/// it reads as somebody else.
pub fn display_name(name: &str, is_own_number: bool) -> String {
    let name = crate::utils::capped_name(&single_line(name));
    if is_own_number {
        format!("{name} (You)")
    } else {
        name
    }
}

/// Collapse whitespace so a multi-line message cannot stretch a fixed row.
fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_core::{ComposingKind, MediaContent, Typist};
    use std::sync::Arc;

    fn chat(is_group: bool) -> Chat {
        let mut chat = Chat::new(if is_group {
            "group@g.us".to_string()
        } else {
            "5521999999999@s.whatsapp.net".to_string()
        });
        chat.name = "Test".to_string();
        chat
    }

    fn message(from_me: bool, content: &str) -> ChatMessage {
        let mut msg = ChatMessage::new_incoming(
            "ID".to_string(),
            "a@s.whatsapp.net".to_string(),
            content.to_string(),
        );
        msg.is_from_me = from_me;
        msg
    }

    fn audio(duration_secs: Option<u32>) -> MediaContent {
        MediaContent {
            media_type: MediaType::Audio,
            data: Arc::new(Vec::new()),
            cache_key: None,
            mime_type: "audio/ogg".to_string(),
            width: None,
            height: None,
            caption: None,
            file_name: None,
            downloadable: None,
            is_animated: false,
            duration_secs,
            data_is_preview: false,
            waveform: None,
        }
    }

    fn typing(name: &str) -> TypingSummary {
        TypingSummary {
            typists: vec![Typist {
                jid: format!("{name}@s.whatsapp.net"),
                name: name.to_string(),
            }],
            total: 1,
            kind: ComposingKind::Text,
        }
    }

    /// The one row where the name alone is ambiguous: a second number of
    /// yours is saved under a person's name like any other contact.
    #[test]
    fn your_own_number_is_marked_as_yours() {
        let mut chat = chat(false);
        chat.name = "Jlucaso 2".to_string();
        assert_eq!(
            ChatRow::new(&chat, None, None, true).name,
            "Jlucaso 2 (You)"
        );
        assert_eq!(ChatRow::new(&chat, None, None, false).name, "Jlucaso 2");
    }

    #[test]
    fn a_chat_with_nothing_in_it_says_so() {
        let row = ChatRow::new(&chat(false), None, None, false);
        assert_eq!(row.preview, Preview::Empty);
    }

    #[test]
    fn typing_outranks_both_the_draft_and_the_last_message() {
        let mut chat = chat(false);
        chat.messages.push(message(false, "achado n e roubado"));
        let row = ChatRow::new(&chat, Some(typing("Ana")), Some("meio escrito"), false);
        assert!(matches!(row.preview, Preview::Typing(_)));
    }

    #[test]
    fn a_draft_outranks_the_last_message() {
        let mut chat = chat(false);
        chat.messages.push(message(false, "ping"));
        let row = ChatRow::new(&chat, None, Some("meio escrito"), false);
        assert_eq!(row.preview, Preview::Draft("meio escrito".to_string()));
    }

    #[test]
    fn whitespace_only_draft_is_not_a_draft() {
        let mut chat = chat(false);
        chat.messages.push(message(false, "ping"));
        let row = ChatRow::new(&chat, None, Some("   \n "), false);
        assert!(matches!(row.preview, Preview::Message { .. }));
    }

    #[test]
    fn a_group_names_the_sender_and_a_direct_chat_does_not() {
        let mut group = chat(true);
        let mut msg = message(false, "achado n e roubado");
        msg.sender_name = Some("Ana".to_string());
        group.messages.push(msg.clone());
        let Preview::Message { prefix, .. } = ChatRow::new(&group, None, None, false).preview
        else {
            panic!("expected a message preview");
        };
        assert_eq!(prefix.as_deref(), Some("Ana"));

        let mut direct = chat(false);
        direct.messages.push(msg);
        let Preview::Message { prefix, .. } = ChatRow::new(&direct, None, None, false).preview
        else {
            panic!("expected a message preview");
        };
        assert_eq!(prefix, None, "there is only one person it could be");
    }

    #[test]
    fn our_own_last_message_carries_its_tick() {
        let mut chat = chat(false);
        let mut msg = message(true, "pong");
        msg.status = MessageStatus::Read;
        chat.messages.push(msg);
        let Preview::Message { status, prefix, .. } =
            ChatRow::new(&chat, None, None, false).preview
        else {
            panic!("expected a message preview");
        };
        assert_eq!(status, Some(MessageStatus::Read));
        assert_eq!(prefix, None, "the tick already says it is ours");
    }

    #[test]
    fn a_voice_note_is_named_and_timed_rather_than_bracketed() {
        let mut chat = chat(false);
        let mut msg = message(false, "");
        msg.media = Some(audio(Some(14)));
        chat.messages.push(msg);
        let Preview::Message { text, glyph, .. } = ChatRow::new(&chat, None, None, false).preview
        else {
            panic!("expected a message preview");
        };
        assert_eq!(text, "Voice message · 0:14");
        assert_eq!(glyph, Some(PreviewGlyph::Audio));
    }

    #[test]
    fn a_caption_is_shown_instead_of_the_media_label() {
        let mut chat = chat(false);
        let mut msg = message(false, "olha isso");
        msg.media = Some(audio(Some(14)));
        chat.messages.push(msg);
        let Preview::Message { text, glyph, .. } = ChatRow::new(&chat, None, None, false).preview
        else {
            panic!("expected a message preview");
        };
        assert_eq!(text, "olha isso");
        assert_eq!(glyph, Some(PreviewGlyph::Audio), "the glyph still applies");
    }

    #[test]
    fn a_multi_line_message_cannot_stretch_the_row() {
        let mut chat = chat(false);
        chat.messages
            .push(message(false, "line one\nline two\ttab"));
        let Preview::Message { text, .. } = ChatRow::new(&chat, None, None, false).preview else {
            panic!("expected a message preview");
        };
        assert_eq!(text, "line one line two tab");
    }

    #[test]
    fn a_manual_unread_has_no_number_to_show() {
        let mut chat = chat(false);
        chat.manually_unread = true;
        assert_eq!(
            ChatRow::new(&chat, None, None, false).unread,
            Unread::Marked
        );

        chat.unread_count = 3;
        assert_eq!(
            ChatRow::new(&chat, None, None, false).unread,
            Unread::Count(3),
            "a real count wins over the sentinel"
        );
    }
}
