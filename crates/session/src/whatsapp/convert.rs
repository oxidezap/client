//! The store and the wire, read as what a front end draws — and back.
//!
//! One-way translations with no state between them: a stored row into a
//! bubble, the store's durable delivery state into the ticks the UI draws, a
//! quote the front end composed into the context info that threads the reply,
//! and the device store into who this device is linked as.
//! [`mark_unread_tail`] is here because it is the correction the first of them
//! owes every caller that hydrates a page.

use std::sync::Arc;

use oxidezap_core::{ChatMessage, MessageStatus, PollContent, UiEvent};
use whatsapp_rust::client::Client;
use whatsapp_rust::wacore::proto_helpers::MessageExt;
use whatsapp_rust::waproto::whatsapp as wa;

use super::media;
use crate::quoting::quoted_from;

/// Un-read the newest `unread` incoming rows of a hydrated page.
///
/// [`stored_to_chat_message`] reads an incoming row back as read, because the
/// store keeps read state on the chat's counter and not on the row — so every
/// caller that hydrates stored rows owes this correction. Skipping it hands a
/// front end a page in which nothing is unread: the read it then asks for
/// names messages the daemon was told were already seen, no receipt goes out,
/// and the badge comes back on the next hydration.
///
/// Returns whatever budget the page did not spend, for a caller walking a
/// PN/LID pair a page at a time.
pub(super) fn mark_unread_tail(messages: &mut [ChatMessage], unread: u32) -> u32 {
    let mut remaining = unread;
    for msg in messages.iter_mut().rev() {
        if remaining == 0 {
            break;
        }
        if !msg.is_from_me {
            msg.is_read = false;
            remaining -= 1;
        }
    }
    remaining
}

/// Convert a durable store row into the UI message model. Media stays
/// download-on-demand (the encoded proto lives in the store if needed later).
pub(super) fn stored_to_chat_message(stored: oxidezap_chat_store::StoredMessage) -> ChatMessage {
    // The stored proto still carries the media envelope: hydrate thumbnails +
    // download info so historical media renders and stays fetchable, instead
    // of degrading to a [kind] text row until a live redelivery.
    let media = (!stored.revoked)
        .then_some(stored.message.as_deref())
        .flatten()
        .and_then(|m| media::media_of(m.get_base_message(), None));
    let poll = (!stored.revoked)
        .then_some(stored.message.as_deref())
        .flatten()
        .and_then(|m| poll_of(m.get_base_message()));
    let content = match (&stored.text, stored.revoked) {
        (_, true) => "[Message deleted]".to_string(),
        (Some(text), _) => text.clone(),
        (None, _) if poll.is_some() => poll
            .as_ref()
            .map(|p| p.question.clone())
            .unwrap_or_default(),
        (None, _) if media.is_some() => String::new(),
        (None, _) => format!("[{}]", stored.kind.as_str()),
    };
    // Outgoing ticks come from the stored delivery status; incoming default
    // to read and load_history un-reads the chat's unread tail (per-incoming
    // read state lives on the chat cursor, not the row).
    let is_read = if stored.from_me {
        matches!(
            stored.status,
            oxidezap_chat_store::MessageStatus::Read | oxidezap_chat_store::MessageStatus::Played
        )
    } else {
        true
    };
    let quoted = (!stored.revoked)
        .then_some(stored.message.as_deref())
        .flatten()
        .and_then(|m| quoted_from(m.get_base_message()));
    ChatMessage {
        id: stored.id,
        sender: stored.sender_jid.to_string(),
        sender_name: None,
        content,
        timestamp: stored.timestamp,
        is_from_me: stored.from_me,
        is_read,
        media,
        reactions: std::collections::HashMap::new(),
        // The store has tracked the real delivery state all along; the UI used
        // to flatten it to a bool and lose the delivered/read distinction that
        // the second tick exists to show.
        status: if stored.from_me {
            store_status(stored.status)
        } else {
            MessageStatus::default()
        },
        quoted,
        revoked: stored.revoked,
        system: None,
        poll,
    }
}

/// A stored poll creation as the bubble's votable content.
///
/// Reads the same creation variants the vote resolves, so the bubble and
/// the ballot never disagree about which options exist.
pub(super) fn poll_of(message: &wa::Message) -> Option<PollContent> {
    let creation = poll_creation_of(message)?;
    Some(PollContent {
        question: creation.name.clone().unwrap_or_default(),
        // One entry per raw option, unnamed ones kept as empty
        // placeholders: filtering them out would shift every later option's
        // index, and the bubble votes by index into this list while the
        // session votes by index into the raw one. An empty slot stays
        // non-votable through `vote_poll`'s missing-name validation, and
        // the bubble draws nothing to tap for it.
        options: creation
            .options
            .iter()
            .map(|o| o.option_name.clone().unwrap_or_default())
            .collect(),
        selectable_count: creation.selectable_options_count.unwrap_or(1).max(1),
    })
}

/// The poll creation a message carries, in any version the store files as
/// a poll.
///
/// One spelling shared by the bubble and the ballot: `poll_of` draws from
/// it and `vote_poll` votes against it, so a variant added here reaches
/// both and a variant missing here misleads neither. v3 first, then v2,
/// then v1 — all three are the same struct, and v3 is what current clients
/// send. v4 is a future-proof wrapper around the same creation, unwrapped
/// for the reason `materialize` peels it: `get_base_message` does not open
/// it, so without this a v4 poll is a creation no lookup below can see.
/// v5/v6 stay out: the classifier files them as unknown rather than polls,
/// and teaching the bubble to draw one while the vote refuses it would be
/// the disagreement this sharing exists to prevent.
pub(super) fn poll_creation_of(message: &wa::Message) -> Option<&wa::message::PollCreationMessage> {
    oxidezap_chat_store::supported_poll_creation_message(message)
}

/// Map the store's durable delivery state onto the one the UI draws.
fn store_status(status: oxidezap_chat_store::MessageStatus) -> MessageStatus {
    use oxidezap_chat_store::MessageStatus as Stored;
    match status {
        // Error is terminal for from_me rows (a nack or a local send failure),
        // so hydration restores the failure indicator rather than grey ticks.
        Stored::Error => MessageStatus::Failed,
        Stored::Pending => MessageStatus::Pending,
        Stored::ServerAck => MessageStatus::Sent,
        Stored::Delivered => MessageStatus::Delivered,
        // Played is Read plus "and listened to it"; the ticks are the same.
        Stored::Read | Stored::Played => MessageStatus::Read,
    }
}

/// The reply context for a quote the front end composed.
///
/// The quoted copy is rebuilt from the preview rather than kept: nothing
/// stores the original protobuf, and the preview is what the quote bar shows
/// on both sides. Its id and its author are what actually thread the reply,
/// and those are exact.
pub(super) fn quote_context(quoted: &oxidezap_core::QuotedMessage) -> wa::ContextInfo {
    use oxidezap_core::QuotedKind;
    use whatsapp_rust::buffa::MessageField;

    let caption = (!quoted.preview.is_empty()).then(|| quoted.preview.clone());
    // The body's *kind*, not a sentence about it. Rebuilding every quote as
    // plain text sent the recipient the word "Photo" where their client would
    // have drawn a photo — and `QuotedKind` exists precisely to carry that
    // distinction across a preview that cannot.
    let original = match quoted.kind {
        Some(QuotedKind::Image) => wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                caption,
                ..Default::default()
            }),
            ..Default::default()
        },
        Some(QuotedKind::Video) => wa::Message {
            video_message: MessageField::some(wa::message::VideoMessage {
                caption,
                ..Default::default()
            }),
            ..Default::default()
        },
        Some(QuotedKind::Audio) => wa::Message {
            audio_message: MessageField::some(wa::message::AudioMessage::default()),
            ..Default::default()
        },
        Some(QuotedKind::Document) => wa::Message {
            document_message: MessageField::some(wa::message::DocumentMessage {
                caption,
                ..Default::default()
            }),
            ..Default::default()
        },
        Some(QuotedKind::Sticker) => wa::Message {
            sticker_message: MessageField::some(wa::message::StickerMessage::default()),
            ..Default::default()
        },
        None => wa::Message {
            conversation: Some(quoted.preview.clone()),
            ..Default::default()
        },
    };
    whatsapp_rust::wacore::proto_helpers::build_quote_context(
        quoted.message_id.clone(),
        quoted.sender.clone(),
        &original,
    )
}

/// Who this device is linked as, off the device store.
///
/// Both fields are optional because both can genuinely be unknown: a device
/// that has paired but never synced its profile has no push name, and the
/// account row says so rather than inventing one.
pub(super) fn account_event(client: &Arc<Client>) -> UiEvent {
    let device = client.persistence_manager().get_device_snapshot();
    UiEvent::AccountUpdated {
        name: Some(device.push_name.clone()).filter(|name| !name.is_empty()),
        jid: device.pn.as_ref().map(ToString::to_string),
        lid: device.lid.as_ref().map(ToString::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatsapp_rust::buffa::MessageField;

    fn stored_poll_creation() -> oxidezap_chat_store::StoredMessage {
        let proto = wa::Message {
            poll_creation_message_v3: MessageField::some(wa::message::PollCreationMessage {
                name: Some("Onde jantamos?".into()),
                options: ["Centro", "Praia"]
                    .iter()
                    .map(|name| wa::message::poll_creation_message::Option {
                        option_name: Some((*name).to_owned()),
                        ..Default::default()
                    })
                    .collect(),
                selectable_options_count: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        };
        oxidezap_chat_store::StoredMessage {
            chat_jid: "559900000001-1620000000@g.us".parse().unwrap(),
            id: "3EB0C".to_string(),
            sender_jid: "559900000001@s.whatsapp.net".parse().unwrap(),
            from_me: false,
            timestamp: wacore::time::now_utc(),
            kind: oxidezap_chat_store::MessageKind::Poll,
            text: None,
            message: Some(Box::new(proto)),
            status: oxidezap_chat_store::MessageStatus::Delivered,
            starred: false,
            edited_at: None,
            revoked: false,
            seq: 1,
        }
    }

    /// A stored poll creation hydrates as a votable poll, with the question
    /// as its content so previews and search read what the bubble draws.
    #[test]
    fn a_stored_poll_creation_hydrates_as_a_votable_poll() {
        let message = stored_to_chat_message(stored_poll_creation());
        let poll = message.poll.expect("a poll creation hydrates a poll");
        assert_eq!(poll.question, "Onde jantamos?");
        assert_eq!(
            poll.options,
            vec!["Centro".to_string(), "Praia".to_string()]
        );
        assert_eq!(poll.selectable_count, 1);
        assert_eq!(message.content, "Onde jantamos?");
    }

    /// A revoked poll is a tombstone, not a ballot: no options to draw and
    /// nothing to vote on.
    #[test]
    fn a_revoked_poll_hydrates_without_poll_content() {
        let mut stored = stored_poll_creation();
        stored.revoked = true;
        let message = stored_to_chat_message(stored);
        assert!(message.poll.is_none());
        assert_eq!(message.content, "[Message deleted]");
    }

    /// An unnamed raw option keeps its slot as an empty placeholder: the
    /// bubble votes by index into this list while the session votes by
    /// index into the raw one, so filtering it out would silently move
    /// every later option onto the wrong ballot line.
    #[test]
    fn an_unnamed_option_keeps_its_index_as_a_placeholder() {
        let mut stored = stored_poll_creation();
        let proto = stored.message.as_mut().expect("proto");
        let creation = proto
            .poll_creation_message_v3
            .as_option_mut()
            .expect("v3 creation");
        creation.options[0].option_name = None;
        let message = stored_to_chat_message(stored);
        let poll = message.poll.expect("a poll creation hydrates a poll");
        assert_eq!(poll.options.len(), 2);
        assert_eq!(poll.options[0], String::new());
        assert_eq!(poll.options[1], "Praia");
    }

    fn stored_poll_v2() -> oxidezap_chat_store::StoredMessage {
        let mut stored = stored_poll_creation();
        let proto = stored.message.as_mut().expect("proto");
        let creation = proto.poll_creation_message_v3.take().expect("v3 creation");
        proto.poll_creation_message_v2 = MessageField::some(creation);
        stored.kind = oxidezap_chat_store::MessageKind::Poll;
        stored
    }

    /// v2 creations hydrate like v3 ones: same struct, same ballot, and the
    /// vote resolves the same variant.
    #[test]
    fn a_v2_creation_hydrates_as_a_votable_poll() {
        let message = stored_to_chat_message(stored_poll_v2());
        let poll = message.poll.expect("a v2 creation hydrates a poll");
        assert_eq!(poll.question, "Onde jantamos?");
        assert_eq!(poll.options.len(), 2);
    }

    /// A v4 future-proof wrapper opens onto the creation inside:
    /// `get_base_message` does not peel it, so without this the poll
    /// renders with no options despite being stored as one.
    #[test]
    fn a_v4_wrapped_creation_hydrates_as_a_votable_poll() {
        let mut stored = stored_poll_v2();
        let proto = stored.message.take().expect("proto");
        stored.message = Some(Box::new(wa::Message {
            poll_creation_message_v4: MessageField::some(wa::message::FutureProofMessage {
                message: MessageField::some(*proto),
            }),
            ..Default::default()
        }));
        let message = stored_to_chat_message(stored);
        let poll = message.poll.expect("a v4 creation hydrates a poll");
        assert_eq!(poll.question, "Onde jantamos?");
        assert_eq!(poll.options.len(), 2);
    }
}
