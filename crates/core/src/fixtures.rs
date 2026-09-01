//! Synthetic chats and messages, for the tests of every crate that depends on
//! this one.
//!
//! Seven modules had grown their own `fn message(...)`, three of them the same
//! thirteen-field literal spelled out again — and a literal is exactly what
//! goes stale when a field is added: the compiler names every one of the seven
//! sites, and whoever adds the field answers the same question seven times.
//! Here it is answered once.
//!
//! Deliberately not a builder DSL. These are plain functions returning a value
//! whose fields are all public, so a test that wants something else says so:
//!
//! ```
//! # use oxidezap_core::fixtures;
//! let mut incoming = fixtures::message("MSG-1", fixtures::PEER, "olá");
//! incoming.is_read = true;
//! ```
//!
//! Every identifier here is invented. No fixture in this workspace may carry a
//! number, a name or a body taken from a real capture, and a shared fixture is
//! the one place a leak would spread from.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::chat::{Chat, ChatMessage, MediaContent, MediaType};

/// A one-to-one contact, and the same three identities the chat store's own
/// tests have always used — one set workspace-wide, so a fixture that moves
/// between crates keeps meaning the same person.
///
/// None of them can be anybody: the subscriber part is a run of zeros, and a
/// real number on that country code is nine digits beginning with a nine.
pub const PEER: &str = "559900000001@s.whatsapp.net";

/// The same contact under their LID identity.
pub const PEER_LID: &str = "111000011112222@lid";

/// A group. The suffix is what makes a JID a group, and the digits are a
/// counted-up placeholder rather than a captured one.
pub const GROUP: &str = "120363000000000001@g.us";

/// The instant every fixture is dated from, in seconds: 2023-11-14T22:13:20Z.
///
/// A fixed point rather than the clock, because a test that compares two rows
/// wants to know their order and not what time it is.
pub const BASE_SECS: i64 = 1_700_000_000;

/// `BASE_SECS + offset` as an instant, which is how a fixture spells "later".
pub fn at(offset_secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(BASE_SECS + offset_secs, 0).expect("a representable fixture instant")
}

/// A message somebody else sent, dated [`BASE_SECS`].
///
/// Read rather than unread and delivered rather than pending are *not* the
/// defaults: an arriving message is neither, and a test that wants either says
/// so on the value it gets back.
pub fn message(id: &str, sender: &str, text: &str) -> ChatMessage {
    let mut message =
        ChatMessage::new_incoming(id.to_string(), sender.to_string(), text.to_string());
    message.timestamp = at(0);
    message
}

/// A message the reader sent, dated [`BASE_SECS`]. Its `status` is `Pending`,
/// which is where an outgoing message starts.
pub fn outgoing(id: &str, text: &str) -> ChatMessage {
    let mut message = ChatMessage::new_outgoing(id.to_string(), text.to_string());
    message.timestamp = at(0);
    message
}

/// Media of `media_type` with no bytes in hand and nothing to fetch — the
/// shape a row carries before anything is downloaded.
///
/// Built through the constructors on [`MediaContent`] rather than a literal,
/// so a fixture cannot describe a combination the real ones never produce (a
/// video whose poster claims to be the full file, an audio row with no
/// duration).
pub fn media(media_type: MediaType) -> MediaContent {
    match media_type {
        MediaType::Image => MediaContent::image(Arc::new(Vec::new()), "image/jpeg".into(), false),
        MediaType::Sticker => {
            MediaContent::sticker(Arc::new(Vec::new()), "image/webp".into(), false, false)
        }
        MediaType::Video => MediaContent::video(Arc::new(Vec::new()), Some(12)),
        MediaType::Audio => {
            MediaContent::audio(Arc::new(Vec::new()), "audio/ogg".into(), Some(3), None)
        }
        MediaType::Document => {
            MediaContent::document("application/pdf".into(), Some("fixture.pdf".into()))
        }
    }
}

/// A photo whose bytes are in hand, as a decoded row carries them.
pub fn image(bytes: Vec<u8>) -> MediaContent {
    MediaContent::image(Arc::new(bytes), "image/jpeg".into(), false)
}

/// A chat with `unread` unread messages and nothing in it.
///
/// The name is the one [`Chat::new`] derives from the JID, which is what the
/// front end shows until a push name or group metadata arrives.
pub fn chat(jid: &str, unread: u32) -> Chat {
    let mut chat = Chat::new(jid.to_string());
    chat.unread_count = unread;
    chat
}

/// A chat holding `messages`, with the preview and activity timestamp the
/// newest of them implies — the shape a store load hands back.
pub fn chat_with(jid: &str, unread: u32, messages: Vec<ChatMessage>) -> Chat {
    let mut chat = self::chat(jid, unread);
    chat.last_message = messages.last().map(|message| message.content.clone());
    chat.last_message_time = messages.last().map(|message| message.timestamp);
    chat.messages = messages;
    chat
}

/// A group chat that knows what to call `participants`, keyed by JID.
pub fn group(jid: &str, participants: HashMap<String, String>) -> Chat {
    let mut chat = self::chat(jid, 0);
    chat.participants = participants;
    chat
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixtures are compared to each other, so the instants they carry have to
    /// be the same on every run and ordered by the offset asked for.
    #[test]
    fn a_fixture_is_dated_by_its_offset_not_by_the_clock() {
        assert_eq!(message("A", PEER, "oi").timestamp, at(0));
        assert!(at(0) < at(1));
        assert_eq!(at(0).timestamp(), BASE_SECS);
    }

    /// The JIDs decide what a chat *is*, and a fixture that got them wrong
    /// would quietly test the one-to-one path everywhere.
    #[test]
    fn the_group_jid_makes_a_group_and_the_peer_jid_does_not() {
        assert!(chat(GROUP, 0).is_group);
        assert!(!chat(PEER, 0).is_group);
        assert_eq!(
            chat_with(PEER, 0, vec![message("A", PEER, "oi")])
                .last_message
                .as_deref(),
            Some("oi")
        );
    }

    /// Media built here goes through the same constructors production does, so
    /// a fixture cannot describe a row that could not arrive.
    #[test]
    fn fixture_media_carries_what_its_kind_implies() {
        assert!(media(MediaType::Audio).duration_secs.is_some());
        assert!(!media(MediaType::Video).has_data());
        assert!(image(vec![1, 2, 3]).has_still_image());
    }
}
