//! Pure event-to-row transforms: what a `wa::Message` means for the tables.
//! No I/O here so every rule is unit-testable without a database.

use wacore::proto_helpers::MessageExt;
use wacore::types::events::UnavailableType;
use waproto::whatsapp as wa;

use crate::types::MessageKind;

/// What the writer should do with one inbound message.
#[derive(Debug)]
pub(crate) enum MessageOp {
    /// Regular content: insert (or refresh) a message row.
    Store {
        kind: &'static str,
        text: Option<String>,
    },
    /// A reaction to another message. `emoji` empty means "remove my reaction".
    Reaction { target_id: String, emoji: String },
    /// An edit of another message: replace its content in place.
    Edit {
        target_id: String,
        new_text: Option<String>,
        new_kind: &'static str,
        new_proto: Vec<u8>,
        sender_timestamp_ms: Option<i64>,
    },
    /// A revoke of another message: tombstone it. These are the raw key's
    /// `fromMe` and participant, not a resolved local author. The live relay
    /// path translates the sending account's perspective before row lookup;
    /// history has a separate producer and must not inherit that translation.
    Revoke {
        target_id: String,
        target_from_me: bool,
        target_participant: Option<String>,
    },
    /// A deliberately shared group-history attachment, persisted before ACK.
    HistoryBundle,
    /// Protocol/bookkeeping payloads that don't belong in a chat log.
    Ignore,
}

/// Content class of the unwrapped message, as a database label (the typed
/// read-model view is [`MessageKind`](crate::types::MessageKind)). Coarser
/// than the proto (one label per renderable bubble type), finer than WA's
/// stanza `type` attribute (which collapses everything to text/media).
pub fn message_kind(message: &wa::Message) -> &'static str {
    let base = normalized_message(message);
    if base.conversation.is_some() || base.extended_text_message.is_set() {
        "text"
    } else if base.image_message.is_set() {
        "image"
    } else if base.ptv_message.is_set() {
        "ptv"
    } else if base.video_message.is_set() {
        "video"
    } else if let Some(audio) = base.audio_message.as_option() {
        if audio.ptt.unwrap_or(false) {
            "ptt"
        } else {
            "audio"
        }
    } else if base.sticker_message.is_set() || base.lottie_sticker_message.is_set() {
        "sticker"
    } else if base.document_message.is_set() {
        "document"
    } else if base.contact_message.is_set() || base.contacts_array_message.is_set() {
        "contact"
    } else if base.location_message.is_set() || base.live_location_message.is_set() {
        "location"
    } else if poll_creation_message(message).is_some() {
        "poll"
    } else if base.message_history_notice.is_set() {
        "history_notice"
    } else if base.album_message.is_set() {
        "album"
    } else if base.product_message.is_set() {
        "product"
    } else if base.event_message.is_set() {
        "event"
    } else if base.group_invite_message.is_set() {
        "group_invite"
    } else if base.template_message.is_set() || base.highly_structured_message.is_set() {
        "template"
    } else if base.template_button_reply_message.is_set() {
        "template_reply"
    } else if base.buttons_message.is_set() {
        "buttons"
    } else if base.buttons_response_message.is_set() {
        "buttons_response"
    } else if base.list_message.is_set() {
        "list"
    } else if base.list_response_message.is_set() {
        "list_response"
    } else if base.interactive_message.is_set() {
        "interactive"
    } else if base.interactive_response_message.is_set() {
        "interactive_response"
    } else {
        "unknown"
    }
}

/// Kind label for a placeholder row of a message we could not decrypt.
pub(crate) const KIND_UNDECRYPTABLE: &str = "undecryptable";

/// Kind label for an `<unavailable>` fanout, or `None` when the failure is the
/// ordinary kind a retry can still resolve.
///
/// The three unrecoverable subtypes are content the phone will never hand to a
/// companion, so their rows are permanent by design — a frontend has to render
/// them as their own thing (WA Web's one-time chip, for view-once) rather than
/// as "waiting for this message", and it can only do that if the store keeps
/// the distinction the wire made.
///
/// Deliberately re-stated here instead of forwarding `UnavailableType::as_str`:
/// that string is the wire value and belongs to the protocol, while these are
/// on-disk labels that must not move when a wire attribute is renamed.
pub(crate) fn unavailable_kind(unavailable_type: UnavailableType) -> Option<&'static str> {
    match unavailable_type {
        UnavailableType::ViewOnce => Some(MessageKind::ViewOnce.as_str()),
        UnavailableType::Hosted => Some(MessageKind::Hosted.as_str()),
        UnavailableType::Bot => Some(MessageKind::Bot.as_str()),
        // A plain fanout stays recoverable; PDO may still fill it in.
        UnavailableType::Unknown => None,
    }
}

/// Unwrap message containers using the same precedence for every consumer.
/// Repeating the pass handles valid nested combinations (for example a group
/// mention around an associated child around a future-proof poll).
pub fn normalized_message(message: &wa::Message) -> &wa::Message {
    let mut base = message;
    // A finite budget also handles malformed, deeply nested carriers without
    // unbounded projection work. Each pass borrows the next message.
    for _ in 0..32 {
        let inner = base
            .device_sent_message
            .as_option()
            .and_then(|w| w.message.as_option())
            .or_else(|| {
                [
                    &base.ephemeral_message,
                    &base.view_once_message,
                    &base.view_once_message_v2,
                    &base.view_once_message_v2_extension,
                    &base.document_with_caption_message,
                    &base.edited_message,
                    &base.group_mentioned_message,
                    &base.associated_child_message,
                    &base.poll_creation_message_v4,
                ]
                .into_iter()
                .find_map(|w| w.as_option().and_then(|w| w.message.as_option()))
            });
        match inner {
            Some(inner) => base = inner,
            None => return base,
        }
    }
    base
}

/// Shared poll-creation detector for classification and storage compaction.
pub fn poll_creation_message(message: &wa::Message) -> Option<&wa::message::PollCreationMessage> {
    let base = normalized_message(message);
    base.poll_creation_message_v6
        .as_option()
        .or_else(|| base.poll_creation_message_v5.as_option())
        .or_else(|| {
            base.poll_creation_message_v4
                .as_option()
                .and_then(|v| v.message.as_option())
                .and_then(|m| {
                    let normalized = normalized_message(m);
                    normalized
                        .poll_creation_message
                        .as_option()
                        .or_else(|| normalized.poll_creation_message_v2.as_option())
                        .or_else(|| normalized.poll_creation_message_v3.as_option())
                })
        })
        .or_else(|| base.poll_creation_message_v3.as_option())
        .or_else(|| base.poll_creation_message_v2.as_option())
        .or_else(|| base.poll_creation_message.as_option())
}

/// Legacy textual poll formats currently supported by session voting. V5/V6
/// are still classified and retained, but intentionally do not become votable
/// until their extended option/capability semantics are implemented.
pub fn supported_poll_creation_message(
    message: &wa::Message,
) -> Option<&wa::message::PollCreationMessage> {
    let base = normalized_message(message);
    base.poll_creation_message_v3
        .as_option()
        .or_else(|| base.poll_creation_message_v2.as_option())
        .or_else(|| base.poll_creation_message.as_option())
}

/// Payloads that are bookkeeping for something else, not a message.
///
/// A vote, an encrypted reaction or comment, a pin, a keep-in-chat — each
/// amends a row somewhere rather than adding one. They reach `message_kind`
/// as "unknown" and `has_any_content` as content, so each used to insert a
/// row, raise the chat to the top of the list with a blank preview, and add
/// one to a badge nobody could clear, because no bubble corresponds to it.
pub(crate) fn is_control_carrier(base: &wa::Message) -> bool {
    base.poll_update_message.is_set()
        || base.enc_reaction_message.is_set()
        || base.enc_comment_message.is_set()
        || base.enc_event_response_message.is_set()
        || base.keep_in_chat_message.is_set()
        || base.pin_in_chat_message.is_set()
        || base.sticker_sync_rmr_message.is_set()
        || base.placeholder_message.is_set()
        || base.status_notification_message.is_set()
}

/// Classify one decrypted message into its materialization op.
pub(crate) fn classify(msg: &wa::Message) -> MessageOp {
    let base = normalized_message(msg);

    if let Some(reaction) = base.reaction_message.as_option() {
        let Some(target_id) = reaction.key.as_option().and_then(|k| k.id.clone()) else {
            return MessageOp::Ignore;
        };
        return MessageOp::Reaction {
            target_id,
            emoji: reaction.text.clone().unwrap_or_default(),
        };
    }

    if let Some(pm) = base.protocol_message.as_option() {
        use wa::message::protocol_message::Type as ProtocolType;
        let target_id = pm.key.as_option().and_then(|k| k.id.clone());
        match (pm.r#type, target_id) {
            (Some(ProtocolType::REVOKE), Some(target_id)) => {
                let key = pm.key.as_option();
                return MessageOp::Revoke {
                    target_id,
                    target_from_me: key.and_then(|k| k.from_me).unwrap_or(false),
                    target_participant: key.and_then(|k| k.participant.clone()),
                };
            }
            (Some(ProtocolType::MESSAGE_EDIT), Some(target_id)) => {
                if let Some(edited) = pm.edited_message.as_option() {
                    let edited_base = normalized_message(edited);
                    return MessageOp::Edit {
                        target_id,
                        new_text: extract_text(edited_base),
                        new_kind: message_kind(edited_base),
                        new_proto: waproto::codec::message_to_vec(edited),
                        sender_timestamp_ms: pm.timestamp_ms,
                    };
                }
                return MessageOp::Ignore;
            }
            // Key shares, history-sync notifications, peer data requests, ...
            _ => return MessageOp::Ignore,
        }
    }

    if base.message_history_bundle.is_set() {
        return MessageOp::HistoryBundle;
    }

    if is_control_carrier(base) {
        return MessageOp::Ignore;
    }

    let kind = message_kind(base);
    if kind == "unknown" && !has_any_content(base) {
        // Bare senderKeyDistribution / messageContextInfo carriers.
        return MessageOp::Ignore;
    }
    MessageOp::Store {
        kind,
        text: extract_text(base),
    }
}

/// Text projection for list previews and full-text search: body text or media
/// caption.
pub(crate) fn extract_text(base: &wa::Message) -> Option<String> {
    if let Some(notice) = base.message_history_notice.as_option() {
        let metadata = notice.message_history_metadata.as_option();
        return Some(
            match metadata.and_then(|m| m.message_count).filter(|n| *n >= 0) {
                Some(count) => {
                    let receivers = metadata.map_or(0, |m| m.history_receivers.len());
                    let messages = if count == 1 { "message" } else { "messages" };
                    let members = if receivers == 1 { "member" } else { "members" };
                    format!("Shared {count} earlier {messages} with {receivers} group {members}")
                }
                None => "Shared earlier messages with group members".into(),
            },
        );
    }
    if let Some(business) = crate::business_content(base) {
        let body = if business.body.is_empty() {
            business.header
        } else {
            business.body
        };
        return (!body.is_empty()).then_some(body);
    }
    base.text_content()
        .or_else(|| base.get_caption())
        .map(str::to_owned)
        .or_else(|| crate::shared_contacts_text(base))
}

/// Whether the unwrapped message carries anything a chat log should show.
/// Guards against storing rows for pure-bookkeeping payloads that classify as
/// "unknown". Decided structurally: strip the bookkeeping carriers and check
/// whether ANY other field remains set, so an unclassified-but-real bubble
/// (e.g. a future WA message type) that also carries `message_context_info`
/// is still stored.
fn has_any_content(base: &wa::Message) -> bool {
    let mut probe = base.clone();
    probe.sender_key_distribution_message = Default::default();
    probe.fast_ratchet_key_sender_key_distribution_message = Default::default();
    probe.message_context_info = Default::default();
    // Encoded emptiness, not PartialEq: presence of an empty submessage still
    // costs wire bytes, while buffa's equality folds it into "absent".
    waproto::codec::message_encoded_len(&probe) > 0
}

/// Whether a message is bookkeeping rather than something to draw.
///
/// The classifier's question, asked from outside the store. A live handler
/// that skips only reactions and protocol stubs publishes a poll update, an
/// encrypted reaction, a pin or a keep-in-chat as a `[Media]` bubble with an
/// unread badge on it -- one that disappears on the next hydration, because
/// the store never wrote a row for it. Peels first, for the reason `classify`
/// does: a carrier inside a wrapper `get_base_message` does not open is still
/// a carrier.
#[must_use]
pub fn is_control_only(msg: &wa::Message) -> bool {
    let base = normalized_message(msg);
    is_control_carrier(base) || base.message_history_bundle.is_set()
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffa::MessageField;
    use wacore::proto_helpers::MessageBuilderExt;

    fn key_for(id: &str) -> MessageField<wa::MessageKey> {
        MessageField::some({
            let mut constructed: wa::MessageKey = Default::default();
            constructed.id = Some(id.into());
            constructed
        })
    }

    #[test]
    fn classifies_plain_text() {
        let msg = wa::Message::text("hello");
        match classify(&msg) {
            MessageOp::Store { kind, text } => {
                assert_eq!(kind, "text");
                assert_eq!(text.as_deref(), Some("hello"));
            }
            other => panic!("expected Store, got {other:?}"),
        }
    }

    #[test]
    fn classifies_ephemeral_wrapped_text() {
        let msg = {
            let mut constructed: wa::Message = Default::default();
            constructed.ephemeral_message = MessageField::some({
                let mut constructed: wa::message::FutureProofMessage = Default::default();
                constructed.message = MessageField::from_box(Box::new(wa::Message::text("secret")));
                constructed
            });
            constructed
        };
        match classify(&msg) {
            MessageOp::Store { kind, text } => {
                assert_eq!(kind, "text");
                assert_eq!(text.as_deref(), Some("secret"));
            }
            other => panic!("expected Store, got {other:?}"),
        }
    }

    fn store_result(msg: &wa::Message) -> (&'static str, Option<String>) {
        match classify(msg) {
            MessageOp::Store { kind, text } => (kind, text),
            other => panic!("expected Store, got {other:?}"),
        }
    }

    #[test]
    fn classifies_hydrated_template_via_field() {
        let msg = {
            let mut constructed: wa::Message = Default::default();
            constructed.template_message = MessageField::some({
                let mut constructed: wa::message::TemplateMessage = Default::default();
                constructed.hydrated_template = MessageField::some({
                    let mut constructed: wa::message::template_message::HydratedFourRowTemplate =
                        Default::default();
                    constructed.hydrated_content_text =
                        Some("Dear customer, your bill is ready".into());
                    constructed.hydrated_footer_text = Some("footer stays display-side".into());
                    constructed
                });
                constructed
            });
            constructed
        };
        let (kind, text) = store_result(&msg);
        assert_eq!(kind, "template");
        assert_eq!(text.as_deref(), Some("Dear customer, your bill is ready"));
    }

    #[test]
    fn classifies_hydrated_template_via_format_oneof() {
        use wa::message::template_message::Format;
        let msg = {
            let mut constructed: wa::Message = Default::default();
            constructed.template_message = MessageField::some({
                let mut constructed: wa::message::TemplateMessage = Default::default();
                constructed.format = Some(Format::HydratedFourRowTemplate(Box::new({
                    let mut constructed: wa::message::template_message::HydratedFourRowTemplate =
                        Default::default();
                    constructed.hydrated_content_text = Some("Your OTP is 000000".into());
                    constructed
                })));
                constructed
            });
            constructed
        };
        let (kind, text) = store_result(&msg);
        assert_eq!(kind, "template");
        assert_eq!(text.as_deref(), Some("Your OTP is 000000"));
    }

    /// Non-hydrated template (placeholders not filled): stored as a template
    /// row, just without extractable text.
    #[test]
    fn template_without_hydrated_content_stores_with_null_text() {
        use wa::message::template_message::Format;
        let msg = {
            let mut constructed: wa::Message = Default::default();
            constructed.template_message = MessageField::some({
                let mut constructed: wa::message::TemplateMessage = Default::default();
                constructed.format = Some(Format::FourRowTemplate(Box::default()));
                constructed
            });
            constructed
        };
        let (kind, text) = store_result(&msg);
        assert_eq!(kind, "template");
        assert!(text.is_none());
    }

    #[test]
    fn classifies_buttons_list_and_interactive_bodies() {
        let buttons = {
            let mut constructed: wa::Message = Default::default();
            constructed.buttons_message = MessageField::some({
                let mut constructed: wa::message::ButtonsMessage = Default::default();
                constructed.content_text = Some("Choose an option".into());
                constructed
            });
            constructed
        };
        assert_eq!(
            store_result(&buttons),
            ("buttons", Some("Choose an option".to_owned()))
        );

        let list = {
            let mut constructed: wa::Message = Default::default();
            constructed.list_message = MessageField::some({
                let mut constructed: wa::message::ListMessage = Default::default();
                constructed.description = Some("Pick a plan".into());
                constructed
            });
            constructed
        };
        assert_eq!(
            store_result(&list),
            ("list", Some("Pick a plan".to_owned()))
        );

        let interactive = {
            let mut constructed: wa::Message = Default::default();
            constructed.interactive_message = MessageField::some({
                let mut constructed: wa::message::InteractiveMessage = Default::default();
                constructed.body = MessageField::some({
                    let mut constructed: wa::message::interactive_message::Body =
                        Default::default();
                    constructed.text = Some("Confirm your order".into());
                    constructed
                });
                constructed
            });
            constructed
        };
        assert_eq!(
            store_result(&interactive),
            ("interactive", Some("Confirm your order".to_owned()))
        );
    }

    #[test]
    fn future_button_response_keeps_kind_without_inventing_display_text() {
        // Message.buttonsResponseMessage is field 43. Its nested field 99
        // represents a response arm that this generated schema does not know.
        let bytes = [
            0xda, 0x02, 0x09, 0x9a, 0x06, 0x06, b'f', b'u', b't', b'u', b'r', b'e',
        ];
        let message = waproto::codec::message_decode(&bytes).unwrap();
        assert_eq!(store_result(&message), ("buttons_response", None));
    }

    #[test]
    fn classifies_business_response_messages() {
        use wa::message::buttons_response_message::Response;
        let buttons_resp = {
            let mut constructed: wa::Message = Default::default();
            constructed.buttons_response_message = MessageField::some({
                let mut constructed: wa::message::ButtonsResponseMessage = Default::default();
                constructed.response = Some(Response::SelectedDisplayText("Yes, confirm".into()));
                constructed
            });
            constructed
        };
        assert_eq!(
            store_result(&buttons_resp),
            ("buttons_response", Some("Yes, confirm".to_owned()))
        );

        let list_resp = {
            let mut constructed: wa::Message = Default::default();
            constructed.list_response_message = MessageField::some({
                let mut constructed: wa::message::ListResponseMessage = Default::default();
                constructed.title = Some("Basic plan".into());
                constructed
            });
            constructed
        };
        assert_eq!(
            store_result(&list_resp),
            ("list_response", Some("Basic plan".to_owned()))
        );

        let tpl_reply = {
            let mut constructed: wa::Message = Default::default();
            constructed.template_button_reply_message = MessageField::some({
                let mut constructed: wa::message::TemplateButtonReplyMessage = Default::default();
                constructed.selected_display_text = Some("Track order".into());
                constructed
            });
            constructed
        };
        assert_eq!(
            store_result(&tpl_reply),
            ("template_reply", Some("Track order".to_owned()))
        );

        let interactive_resp = {
            let mut constructed: wa::Message = Default::default();
            constructed.interactive_response_message = MessageField::some({
                let mut constructed: wa::message::InteractiveResponseMessage = Default::default();
                constructed.body = MessageField::some({
                    let mut constructed: wa::message::interactive_response_message::Body =
                        Default::default();
                    constructed.text = Some("flow reply".into());
                    constructed
                });
                constructed
            });
            constructed
        };
        assert_eq!(
            store_result(&interactive_resp),
            ("interactive_response", Some("flow reply".to_owned()))
        );
    }

    #[test]
    fn classifies_image_with_caption() {
        let msg = {
            let mut constructed: wa::Message = Default::default();
            constructed.image_message = MessageField::some({
                let mut constructed: wa::message::ImageMessage = Default::default();
                constructed.caption = Some("look".into());
                constructed
            });
            constructed
        };
        match classify(&msg) {
            MessageOp::Store { kind, text } => {
                assert_eq!(kind, "image");
                assert_eq!(text.as_deref(), Some("look"));
            }
            other => panic!("expected Store, got {other:?}"),
        }
    }

    #[test]
    fn ptt_and_audio_are_distinct() {
        let ptt = {
            let mut constructed: wa::Message = Default::default();
            constructed.audio_message = MessageField::some({
                let mut constructed: wa::message::AudioMessage = Default::default();
                constructed.ptt = Some(true);
                constructed
            });
            constructed
        };
        let audio = {
            let mut constructed: wa::Message = Default::default();
            constructed.audio_message = MessageField::some(wa::message::AudioMessage::default());
            constructed
        };
        assert_eq!(message_kind(&ptt), "ptt");
        assert_eq!(message_kind(&audio), "audio");
    }

    #[test]
    fn poll_album_and_product_kinds_share_wrapper_normalization() {
        let v5 = {
            let mut constructed: wa::Message = Default::default();
            constructed.poll_creation_message_v5 = MessageField::some(Default::default());
            constructed
        };
        let v6 = {
            let mut constructed: wa::Message = Default::default();
            constructed.poll_creation_message_v6 = MessageField::some(Default::default());
            constructed
        };
        for poll in [&v5, &v6] {
            assert_eq!(message_kind(poll), "poll");
            assert_eq!(MessageKind::of(poll), MessageKind::Poll);
            assert!(supported_poll_creation_message(poll).is_none());
        }

        let album = {
            let mut constructed: wa::Message = Default::default();
            constructed.album_message = MessageField::some(Default::default());
            constructed
        };
        assert_eq!(message_kind(&album), "album");
        let product = {
            let mut constructed: wa::Message = Default::default();
            constructed.product_message = MessageField::some(Default::default());
            constructed
        };
        assert_eq!(message_kind(&product), "product");

        let nested = {
            let mut constructed: wa::Message = Default::default();
            constructed.associated_child_message = MessageField::some({
                let mut constructed: wa::message::FutureProofMessage = Default::default();
                constructed.message = MessageField::some({
                    let mut constructed: wa::Message = Default::default();
                    constructed.group_mentioned_message = MessageField::some({
                        let mut constructed: wa::message::FutureProofMessage = Default::default();
                        constructed.message = MessageField::some(album);
                        constructed
                    });
                    constructed
                });
                constructed
            });
            constructed
        };
        assert_eq!(message_kind(&nested), "album");
    }

    #[test]
    fn classifies_reaction_add_and_remove() {
        let add = {
            let mut constructed: wa::Message = Default::default();
            constructed.reaction_message = MessageField::some({
                let mut constructed: wa::message::ReactionMessage = Default::default();
                constructed.key = key_for("MSG1");
                constructed.text = Some("👍".into());
                constructed
            });
            constructed
        };
        match classify(&add) {
            MessageOp::Reaction { target_id, emoji } => {
                assert_eq!(target_id, "MSG1");
                assert_eq!(emoji, "👍");
            }
            other => panic!("expected Reaction, got {other:?}"),
        }

        let remove = {
            let mut constructed: wa::Message = Default::default();
            constructed.reaction_message = MessageField::some({
                let mut constructed: wa::message::ReactionMessage = Default::default();
                constructed.key = key_for("MSG1");
                constructed.text = Some(String::new());
                constructed
            });
            constructed
        };
        match classify(&remove) {
            MessageOp::Reaction { emoji, .. } => assert!(emoji.is_empty()),
            other => panic!("expected Reaction, got {other:?}"),
        }
    }

    #[test]
    fn reaction_without_target_key_is_ignored() {
        let msg = {
            let mut constructed: wa::Message = Default::default();
            constructed.reaction_message = MessageField::some({
                let mut constructed: wa::message::ReactionMessage = Default::default();
                constructed.text = Some("👍".into());
                constructed
            });
            constructed
        };
        assert!(matches!(classify(&msg), MessageOp::Ignore));
    }

    #[test]
    fn classifies_revoke_and_edit() {
        use wa::message::protocol_message::Type as ProtocolType;
        let revoke = {
            let mut constructed: wa::Message = Default::default();
            constructed.protocol_message = MessageField::some({
                let mut constructed: wa::message::ProtocolMessage = Default::default();
                constructed.key = key_for("MSG2");
                constructed.r#type = Some(ProtocolType::REVOKE);
                constructed
            });
            constructed
        };
        assert!(matches!(
            classify(&revoke),
            MessageOp::Revoke { target_id, .. } if target_id == "MSG2"
        ));

        let edit = {
            let mut constructed: wa::Message = Default::default();
            constructed.protocol_message = MessageField::some({
                let mut constructed: wa::message::ProtocolMessage = Default::default();
                constructed.key = key_for("MSG3");
                constructed.r#type = Some(ProtocolType::MESSAGE_EDIT);
                constructed.edited_message = MessageField::from_box(Box::new({
                    let mut constructed: wa::Message = Default::default();
                    constructed.group_mentioned_message = MessageField::some({
                        let mut constructed: wa::message::FutureProofMessage = Default::default();
                        constructed.message = MessageField::some(wa::Message::text("fixed"));
                        constructed
                    });
                    constructed
                }));
                constructed
            });
            constructed
        };
        match classify(&edit) {
            MessageOp::Edit {
                target_id,
                new_text,
                new_kind,
                new_proto,
                ..
            } => {
                assert_eq!(target_id, "MSG3");
                assert_eq!(new_text.as_deref(), Some("fixed"));
                assert_eq!(new_kind, "text");
                assert!(!new_proto.is_empty());
            }
            other => panic!("expected Edit, got {other:?}"),
        }
    }

    #[test]
    fn bookkeeping_only_messages_are_ignored() {
        let skdm_only = {
            let mut constructed: wa::Message = Default::default();
            constructed.sender_key_distribution_message =
                MessageField::some(wa::message::SenderKeyDistributionMessage::default());
            constructed
        };
        assert!(matches!(classify(&skdm_only), MessageOp::Ignore));

        let key_share = {
            let mut constructed: wa::Message = Default::default();
            constructed.protocol_message = MessageField::some({
                let mut constructed: wa::message::ProtocolMessage = Default::default();
                constructed.r#type =
                    Some(wa::message::protocol_message::Type::APP_STATE_SYNC_KEY_SHARE);
                constructed
            });
            constructed
        };
        assert!(matches!(classify(&key_share), MessageOp::Ignore));
    }

    #[test]
    fn unknown_content_with_context_info_is_still_stored() {
        // A future/unclassified bubble type often carries message_context_info
        // (msg secrets); that must not demote it to bookkeeping-only.
        let msg = {
            let mut constructed: wa::Message = Default::default();
            constructed.send_payment_message =
                MessageField::some(wa::message::SendPaymentMessage::default());
            constructed.message_context_info =
                MessageField::some(wa::MessageContextInfo::default());
            constructed
        };
        match classify(&msg) {
            MessageOp::Store { kind, .. } => assert_eq!(kind, "unknown"),
            other => panic!("expected Store, got {other:?}"),
        }

        // While a PURE bookkeeping payload still has no bubble.
        let carrier_only = {
            let mut constructed: wa::Message = Default::default();
            constructed.sender_key_distribution_message =
                MessageField::some(wa::message::SenderKeyDistributionMessage::default());
            constructed.message_context_info =
                MessageField::some(wa::MessageContextInfo::default());
            constructed
        };
        assert!(matches!(classify(&carrier_only), MessageOp::Ignore));
    }

    #[test]
    fn unclassified_but_present_content_stores_as_unknown() {
        let msg = {
            let mut constructed: wa::Message = Default::default();
            constructed.send_payment_message =
                MessageField::some(wa::message::SendPaymentMessage::default());
            constructed
        };
        match classify(&msg) {
            MessageOp::Store { kind, .. } => assert_eq!(kind, "unknown"),
            other => panic!("expected Store, got {other:?}"),
        }
    }

    /// A poll update, a pin or a keep-in-chat carries nothing to draw. The
    /// live path published one as a `[Media]` bubble with an unread badge,
    /// and the store wrote no row for it, so the bubble and the badge went
    /// away at the next hydration having sent no receipt for either.
    #[test]
    fn a_control_carrier_is_not_a_bubble_on_either_path() {
        let poll = {
            let mut constructed: wa::Message = Default::default();
            constructed.poll_update_message =
                MessageField::some(wa::message::PollUpdateMessage::default());
            constructed
        };
        assert!(is_control_only(&poll));
        assert!(matches!(classify(&poll), MessageOp::Ignore));

        // And inside a wrapper `get_base_message` does not open, which is the
        // half a caller reaching for `get_base_message` alone would miss.
        let wrapped = {
            let mut constructed: wa::Message = Default::default();
            constructed.group_mentioned_message = MessageField::some({
                let mut constructed: wa::message::FutureProofMessage = Default::default();
                constructed.message = MessageField::some(poll);
                constructed
            });
            constructed
        };
        assert!(is_control_only(&wrapped));

        // An ordinary message is not, which is what keeps the skip off the
        // conversation.
        assert!(!is_control_only(&wa::Message::text("oi")));
    }
}
