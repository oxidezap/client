//! One row of a conversation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::media::{MediaContent, MediaType};
use super::{fallback_sender_name, is_false};
use crate::message_status::MessageStatus;
use crate::quoted::QuotedMessage;
use crate::system_notice::SystemNotice;

/// A chat message
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Unique message ID
    pub id: String,
    /// Sender identifier (JID)
    pub sender: String,
    /// Sender's display name (push name, for group chats)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_name: Option<String>,
    /// Message text content
    pub content: String,
    /// When the message was sent/received
    pub timestamp: DateTime<Utc>,
    /// Whether this message was sent by the current user
    pub is_from_me: bool,
    /// Whether the message has been read
    pub is_read: bool,
    /// Optional media content
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaContent>,
    /// Reactions on this message (emoji -> list of sender JIDs)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub reactions: HashMap<String, Vec<String>>,
    /// How far an outgoing message got. Meaningless for an incoming one:
    /// read it through [`Self::delivery`], which returns `None` there rather
    /// than reporting our own send state for someone else's message.
    pub status: MessageStatus,
    /// The message this one replies to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quoted: Option<QuotedMessage>,
    /// Set when nobody typed this: a call record, a group change. Such a row
    /// has no author and no ticks, and renders centred rather than as a bubble.
    /// Whether the sender took this message back.
    ///
    /// Kept as a fact rather than left implicit in the "[Message deleted]"
    /// text it produces: a tombstone is still a row, and the surfaces that
    /// have to know it is one — the status feed, which must not offer a
    /// deleted update to watch — should not have to recognise a sentence.
    #[serde(default, skip_serializing_if = "is_false")]
    pub revoked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemNotice>,
}

/// What a failed message can be sent again as.
///
/// Borrowed from the message, because the common caller only wants to know
/// whether there is anything here at all.
pub enum Resend<'a> {
    Text(&'a str),
    VoiceNote(&'a MediaContent),
}

impl ChatMessage {
    /// What to draw for whoever wrote this: the name somebody has for them,
    /// or the number if nobody has one yet.
    ///
    /// The number is produced here rather than stored in `sender_name`,
    /// because that field only ever gains a value —
    /// [`Chat::update_participant`](super::Chat::update_participant) fills
    /// blanks. A row stamped with a number
    /// could never take the push name that arrives a second later, so the
    /// same person would read as a number on their reloaded bubbles and by
    /// name on their new ones.
    pub fn author_label(&self) -> std::borrow::Cow<'_, str> {
        match &self.sender_name {
            Some(name) => std::borrow::Cow::Borrowed(name),
            None => fallback_sender_name(&self.sender),
        }
    }

    /// Create a new outgoing message
    pub fn new_outgoing(id: String, content: String) -> Self {
        Self {
            id,
            sender: "Me".to_string(),
            sender_name: None,
            content,
            timestamp: wacore::time::now_utc(),
            is_from_me: true,
            is_read: false,
            media: None,
            reactions: HashMap::new(),
            status: MessageStatus::Pending,
            quoted: None,
            revoked: false,
            system: None,
        }
    }

    /// Create a new outgoing message with media
    pub fn new_outgoing_with_media(id: String, content: String, media: MediaContent) -> Self {
        Self {
            media: Some(media),
            ..Self::new_outgoing(id, content)
        }
    }

    /// Create a new incoming message
    #[allow(dead_code)]
    pub fn new_incoming(id: String, sender: String, content: String) -> Self {
        Self {
            id,
            sender,
            sender_name: None,
            content,
            timestamp: wacore::time::now_utc(),
            is_from_me: false,
            is_read: false,
            media: None,
            reactions: HashMap::new(),
            status: MessageStatus::default(),
            quoted: None,
            revoked: false,
            system: None,
        }
    }

    /// How far this message got, or `None` when it is not ours to report.
    pub fn delivery(&self) -> Option<MessageStatus> {
        self.is_from_me.then_some(self.status)
    }

    /// The ticks to draw, given whether this conversation is with your own
    /// number.
    ///
    /// A message to yourself has been read by the only person who could read
    /// it, and every WhatsApp client shows it as read the moment it lands. No
    /// receipt ever arrives to say so — the peer that would send one is this
    /// account — so a status derived from receipts alone sits on one grey tick
    /// for good. The rule lives here rather than in a renderer because the
    /// timeline and the chat list both draw those ticks and would otherwise
    /// have to agree by hand.
    pub fn delivery_in(&self, is_self_chat: bool) -> Option<MessageStatus> {
        let status = self.delivery()?;
        // Not a blanket promotion: a send that is still pending or has failed
        // says something true about this device, and claiming it was read
        // would be a lie about a message that never left.
        if is_self_chat && status.has_left_this_device() {
            return Some(MessageStatus::Read);
        }
        Some(status)
    }

    /// Whether the send failed, which is what the bubble draws in red.
    pub fn is_failed(&self) -> bool {
        self.is_from_me && self.status.is_failed()
    }

    /// What sending this again would put on the wire, if anything.
    ///
    /// One question with one answer, asked by the bubble to decide whether to
    /// offer a retry and by the retry to decide what to send. They were two
    /// separate conditions — "did it fail" and "is there text or opus here" —
    /// so a failed message with neither drew a control that answered a click
    /// with nothing. Text and voice notes are what this client composes, and
    /// therefore what it can compose a second time.
    pub fn resend(&self) -> Option<Resend<'_>> {
        if !self.is_failed() {
            return None;
        }
        if !self.content.is_empty() {
            return Some(Resend::Text(&self.content));
        }
        // A voice note has no text and is not therefore beyond recovery: the
        // failed bubble still holds the encoded opus, its length and its
        // waveform, which is everything the send needs.
        self.media
            .as_ref()
            .filter(|media| media.media_type == MediaType::Audio && !media.data.is_empty())
            .map(Resend::VoiceNote)
    }

    /// Get the preview text for chat list display.
    ///
    /// Returns:
    /// - For text-only messages: the message content
    /// - For media messages: "[MediaType] caption" or just "[MediaType]"
    pub fn preview_text(&self) -> String {
        if let Some(media) = &self.media {
            let label = media.media_type.display_label();
            // Check caption first, then fall back to content
            let caption = media
                .caption
                .as_ref()
                .filter(|c| !c.is_empty())
                .or_else(|| Some(&self.content).filter(|c| !c.is_empty()));

            if let Some(text) = caption {
                format!("{} {}", label, text)
            } else {
                label.to_string()
            }
        } else {
            self.content.clone()
        }
    }

    /// Create a message with media content
    #[allow(dead_code)]
    pub fn with_media(mut self, media: MediaContent) -> Self {
        // Use caption as content if available
        if let Some(caption) = &media.caption
            && !caption.is_empty()
        {
            self.content = caption.clone();
        }
        self.media = Some(media);
        self
    }
}

/// An incoming row at a known second, for the tests in this crate that care
/// about where a message lands rather than what is in it.
///
/// Here rather than in each test module: ordering, hydration and reactions are
/// tested in three files and all three want the same row.
#[cfg(test)]
pub(super) fn make_message(id: &str, timestamp_secs: i64) -> ChatMessage {
    use chrono::TimeZone;

    ChatMessage {
        timestamp: Utc.timestamp_opt(timestamp_secs, 0).unwrap(),
        ..ChatMessage::new_incoming(id.to_string(), "test".to_string(), format!("Message {id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A message to your own number has been read by the only person who could
    /// read it, and no receipt will ever say so — the peer that would send one
    /// is this account. Left to receipts alone the bubble sat on one grey tick
    /// for good.
    #[test]
    fn a_message_to_yourself_is_read_the_moment_it_lands() {
        let mut message = ChatMessage::new_outgoing("m".into(), "hi".into());
        message.status = MessageStatus::Sent;

        assert_eq!(message.delivery_in(false), Some(MessageStatus::Sent));
        assert_eq!(message.delivery_in(true), Some(MessageStatus::Read));
    }

    /// Not a blanket promotion: a send still queued, or one that failed, says
    /// something true about this device. Calling it read would claim a message
    /// was seen that never left.
    #[test]
    fn a_send_that_never_left_is_not_read_even_in_your_own_chat() {
        for status in [MessageStatus::Pending, MessageStatus::Failed] {
            let mut message = ChatMessage::new_outgoing("m".into(), "hi".into());
            message.status = status;
            assert_eq!(message.delivery_in(true), Some(status), "for {status:?}");
        }
    }

    /// A person nobody has named is still somebody. The number is drawn, not
    /// stored: the stored field only ever gains a value, so a row stamped
    /// with a number could never take the name that arrives after it.
    #[test]
    fn an_unnamed_author_is_drawn_as_their_number() {
        let mut message =
            ChatMessage::new_incoming("m".into(), "12025550143@s.whatsapp.net".into(), "hi".into());
        assert_eq!(message.author_label(), "+12025550143");

        message.sender_name = Some("Ana".into());
        assert_eq!(message.author_label(), "Ana");
    }

    /// Their message in your own chat is not yours to have ticks on at all.
    #[test]
    fn an_incoming_message_has_no_ticks_in_any_chat() {
        let mut message = ChatMessage::new_outgoing("m".into(), "hi".into());
        message.is_from_me = false;
        assert_eq!(message.delivery_in(true), None);
    }
}
