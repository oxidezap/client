//! The value a plugin reads through a handle.
//!
//! Nothing here is serialized. A [`UiEvent`] becomes a short list of
//! `(field, value)` pairs once, when it is delivered, and the plugin reads
//! back only what it touches — which is the whole reason the ABI is built on
//! handles rather than on a payload. An autoreply that looks at the text and
//! the chat pays for two strings out of an event that has a dozen fields on
//! it, and a plugin subscribed to a kind it then ignores pays for the match
//! statement below and nothing else.
//!
//! The pairs are a `Vec` and lookup is a linear scan. No event here holds
//! more than a dozen, and a map would cost a hash per read to save a
//! comparison per read.

use oxidezap_core::{MediaType, UiEvent};
use oxidezap_plugin_abi as abi;
use oxidezap_plugin_abi::fields;

/// One field's value, in the three shapes the ABI can answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Int(i64),
    /// A repeated string field, reached through `oxi_field_len` and
    /// `oxi_field_at`.
    List(Vec<String>),
}

/// One delivery: what kind it is, and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub kind: i32,
    fields: Vec<(i32, Value)>,
}

impl Event {
    /// An event of `kind` with nothing on it. Used for the kinds whose whole
    /// content is added by the caller — a timer, a UI action.
    #[must_use]
    pub fn new(kind: i32) -> Self {
        Self {
            kind,
            fields: Vec::new(),
        }
    }

    /// Add a field, skipping an empty string.
    ///
    /// The absence rule from the ABI docs, enforced at the one place a value
    /// enters: an empty string and a missing one must not be distinguishable
    /// by a reader, so the host never stores one that would be.
    pub fn str(mut self, field: i32, value: impl Into<String>) -> Self {
        let value = value.into();
        if !value.is_empty() {
            self.fields.push((field, Value::Str(value)));
        }
        self
    }

    /// Add an integer, skipping a zero — which is what an absent one reads
    /// back as anyway.
    pub fn int(mut self, field: i32, value: i64) -> Self {
        if value != 0 {
            self.fields.push((field, Value::Int(value)));
        }
        self
    }

    /// Add a boolean as `1`. A false one is not stored, by the same rule.
    pub fn flag(self, field: i32, value: bool) -> Self {
        self.int(field, i64::from(value))
    }

    pub fn list(mut self, field: i32, values: Vec<String>) -> Self {
        if !values.is_empty() {
            self.fields.push((field, Value::List(values)));
        }
        self
    }

    /// What `field` holds, if anything.
    #[must_use]
    pub fn get(&self, field: i32) -> Option<&Value> {
        self.fields
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, v)| v)
    }
}

/// Turn a session event into what a plugin sees, or `None` for one no plugin
/// kind covers.
///
/// The mapping is deliberately lossy and coarser than [`UiEvent`]: a plugin
/// asks for "messages", not for each of the events a message produces. What
/// is dropped here is everything a plugin has no way to act on — a QR code it
/// cannot show, a history load it has nowhere to put, a video frame it cannot
/// decode.
#[must_use]
pub fn from_session(event: &UiEvent) -> Option<Event> {
    use fields::{call, connection, media, receipt};

    Some(match event {
        UiEvent::MessageReceived {
            chat_jid,
            message,
            sender_name,
        } => Event::new(abi::kinds::MESSAGE)
            .str(fields::CHAT_JID, chat_jid.clone())
            .flag(fields::IS_GROUP, is_group(chat_jid))
            .str(fields::MESSAGE_ID, message.id.clone())
            // Flattened, because a plugin matching on text should not have to
            // know that `*ping*` is the same word as `ping`. The markers are
            // presentation, and the one surface here that would have to parse
            // them is the one least equipped to.
            .str(
                fields::TEXT,
                oxidezap_core::plain_message_text(&message.content).into_owned(),
            )
            .flag(fields::FROM_ME, message.is_from_me)
            .int(fields::TIMESTAMP_MS, message.timestamp.timestamp_millis())
            // Only in a group. In a one-to-one chat the sender *is* the chat,
            // and repeating it would make "who wrote this" look like a
            // question worth asking there.
            .str(
                fields::SENDER_JID,
                if is_group(chat_jid) {
                    message.sender.clone()
                } else {
                    String::new()
                },
            )
            .str(
                fields::SENDER_NAME,
                sender_name
                    .clone()
                    .or_else(|| message.sender_name.clone())
                    .unwrap_or_default(),
            )
            .flag(fields::REVOKED, message.revoked)
            .int(
                fields::MEDIA_KIND,
                message
                    .media
                    .as_ref()
                    .map_or(media::NONE, |m| match m.media_type {
                        MediaType::Image => media::IMAGE,
                        MediaType::Video => media::VIDEO,
                        MediaType::Audio => media::AUDIO,
                        MediaType::Document => media::DOCUMENT,
                        MediaType::Sticker => media::STICKER,
                    }),
            )
            .str(
                fields::QUOTED_ID,
                message
                    .quoted
                    .as_ref()
                    .map(|q| q.message_id.clone())
                    .unwrap_or_default(),
            ),

        UiEvent::Connected => {
            Event::new(abi::kinds::CONNECTION).int(fields::CONNECTION_STATE, connection::CONNECTED)
        }
        UiEvent::Disconnected(reason) => Event::new(abi::kinds::CONNECTION)
            .int(fields::CONNECTION_STATE, connection::DISCONNECTED)
            .str(fields::REASON, reason.clone()),
        UiEvent::LoggedOut(reason) => Event::new(abi::kinds::CONNECTION)
            .int(fields::CONNECTION_STATE, connection::LOGGED_OUT)
            .str(fields::REASON, reason.clone()),
        // A pair code and a QR are two credentials for one state, and a
        // plugin has no way to show either. What it can act on is that the
        // account is not usable yet.
        UiEvent::QrCode { .. } | UiEvent::PairCode { .. } => {
            Event::new(abi::kinds::CONNECTION).int(fields::CONNECTION_STATE, connection::PAIRING)
        }
        UiEvent::PairSuccess => {
            Event::new(abi::kinds::CONNECTION).int(fields::CONNECTION_STATE, connection::SYNCING)
        }

        UiEvent::ReceiptReceived {
            chat_jid,
            message_ids,
            receipt_type,
        } => Event::new(abi::kinds::RECEIPT)
            .str(fields::CHAT_JID, chat_jid.clone())
            .flag(fields::IS_GROUP, is_group(chat_jid))
            .int(
                fields::RECEIPT_KIND,
                match receipt_type {
                    oxidezap_core::ReceiptType::Read | oxidezap_core::ReceiptType::ReadSelf => {
                        receipt::READ
                    }
                    oxidezap_core::ReceiptType::Played | oxidezap_core::ReceiptType::PlayedSelf => {
                        receipt::PLAYED
                    }
                    // Delivered, and anything the library adds later. A
                    // receipt kind this host does not recognise is still a
                    // receipt, and reporting the weakest of them is the
                    // reading that cannot overstate what happened.
                    _ => receipt::DELIVERED,
                },
            )
            .list(fields::MESSAGE_IDS, message_ids.clone()),

        UiEvent::ReactionReceived {
            chat_jid,
            message_id,
            sender,
            emoji,
        } => Event::new(abi::kinds::REACTION)
            .str(fields::CHAT_JID, chat_jid.clone())
            .flag(fields::IS_GROUP, is_group(chat_jid))
            .str(fields::MESSAGE_ID, message_id.clone())
            .str(fields::SENDER_JID, sender.clone())
            .str(fields::EMOJI, emoji.clone()),

        UiEvent::ChatPresence {
            chat_jid,
            sender_jid,
            sender_name,
            composing,
        } => Event::new(abi::kinds::PRESENCE)
            .str(fields::CHAT_JID, chat_jid.clone())
            .flag(fields::IS_GROUP, is_group(chat_jid))
            .str(fields::SENDER_JID, sender_jid.clone())
            .str(fields::SENDER_NAME, sender_name.clone().unwrap_or_default())
            .flag(fields::COMPOSING, composing.is_some()),

        UiEvent::IncomingCall(call) => Event::new(abi::kinds::CALL)
            .str(fields::CALL_ID, call.call_id.clone())
            .int(fields::CALL_EVENT, call::INCOMING)
            .flag(fields::CALL_IS_VIDEO, call.is_video)
            .str(fields::PEER_JID, call.caller_jid.clone()),
        UiEvent::OutgoingCallStarted {
            call_id,
            recipient_jid,
            is_video,
            ..
        } => Event::new(abi::kinds::CALL)
            .str(fields::CALL_ID, call_id.clone())
            .int(fields::CALL_EVENT, call::OUTGOING)
            .flag(fields::CALL_IS_VIDEO, *is_video)
            .str(fields::PEER_JID, recipient_jid.clone()),
        UiEvent::CallAnswered { call_id, is_video } => Event::new(abi::kinds::CALL)
            .str(fields::CALL_ID, call_id.clone())
            .int(fields::CALL_EVENT, call::ANSWERED)
            .flag(fields::CALL_IS_VIDEO, *is_video),
        // Both ways a call can stop being one. `CallEndedElsewhere` is the
        // same fact to a plugin, which has no local record to correct.
        UiEvent::CallEnded(call_id) | UiEvent::CallEndedElsewhere(call_id) => {
            Event::new(abi::kinds::CALL)
                .str(fields::CALL_ID, call_id.clone())
                .int(fields::CALL_EVENT, call::ENDED)
        }

        _ => return None,
    })
}

/// Whether a JID names a group conversation.
///
/// Derived here rather than carried, because it is a property of the
/// identifier and the session event does not say. A plugin filtering "only
/// answer in one-to-one chats" is the first thing anyone writes, and making
/// it parse a JID would put a second, worse JID parser in every plugin.
fn is_group(jid: &str) -> bool {
    jid.ends_with("@g.us")
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_core::{ChatMessage, MessageStatus};

    fn message(chat: &str, text: &str, from_me: bool) -> UiEvent {
        UiEvent::MessageReceived {
            chat_jid: chat.into(),
            message: Box::new(ChatMessage {
                id: "MSG1".into(),
                sender: "5511999@s.whatsapp.net".into(),
                sender_name: Some("Ana".into()),
                content: text.into(),
                timestamp: chrono::DateTime::from_timestamp_millis(1_700_000_000_000)
                    .expect("a valid instant"),
                is_from_me: from_me,
                is_read: false,
                media: None,
                reactions: Default::default(),
                status: MessageStatus::Delivered,
                quoted: None,
                revoked: false,
                system: None,
            }),
            sender_name: None,
        }
    }

    #[test]
    fn a_message_carries_what_a_filter_needs() {
        let ev = from_session(&message("5511999@s.whatsapp.net", "ping", false)).expect("mapped");
        assert_eq!(ev.kind, abi::kinds::MESSAGE);
        assert_eq!(
            ev.get(fields::CHAT_JID),
            Some(&Value::Str("5511999@s.whatsapp.net".into()))
        );
        assert_eq!(ev.get(fields::TEXT), Some(&Value::Str("ping".into())));
        assert_eq!(
            ev.get(fields::TIMESTAMP_MS),
            Some(&Value::Int(1_700_000_000_000))
        );
    }

    /// The absence rule, at the one place values enter: false and zero are
    /// never stored, so a reader cannot tell "not set" from "set to the
    /// default" — which is exactly the guarantee the ABI makes.
    #[test]
    fn a_default_is_not_stored() {
        let ev = from_session(&message("5511999@s.whatsapp.net", "ping", false)).expect("mapped");
        assert_eq!(ev.get(fields::FROM_ME), None);
        assert_eq!(ev.get(fields::REVOKED), None);
        assert_eq!(ev.get(fields::MEDIA_KIND), None);
        // And a one-to-one chat says nothing about a sender, because the
        // sender is the chat.
        assert_eq!(ev.get(fields::SENDER_JID), None);
        assert_eq!(ev.get(fields::IS_GROUP), None);
    }

    #[test]
    fn a_group_names_its_sender() {
        let ev = from_session(&message("120363@g.us", "oi", false)).expect("mapped");
        assert_eq!(ev.get(fields::IS_GROUP), Some(&Value::Int(1)));
        assert_eq!(
            ev.get(fields::SENDER_JID),
            Some(&Value::Str("5511999@s.whatsapp.net".into()))
        );
    }

    /// Emphasis markers are presentation. A plugin matching on words should
    /// not have to know that `*ping*` is the same word.
    #[test]
    fn text_arrives_without_its_markup() {
        let ev = from_session(&message("a@s.whatsapp.net", "*ping* me", false)).expect("mapped");
        assert_eq!(ev.get(fields::TEXT), Some(&Value::Str("ping me".into())));
    }

    #[test]
    fn an_event_no_kind_covers_is_not_delivered() {
        assert!(from_session(&UiEvent::InitComplete).is_none());
        assert!(from_session(&UiEvent::Error("boom".into())).is_none());
    }
}
