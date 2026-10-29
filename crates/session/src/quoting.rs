//! Reading the reply context off an inbound message.
//!
//! WhatsApp carries a quote in the `ContextInfo` attached to whichever body
//! the reply happens to use. The library keeps its exhaustive list of
//! context-carrying variants private, so this walks the bodies a reply is
//! actually ever sent as. Anything outside that set simply renders without a
//! quote bar, which is the same thing that happens when the field is absent.

use oxidezap_core::{QuotedKind, QuotedMessage};
use whatsapp_rust::wacore::proto_helpers::MessageExt as _;
use whatsapp_rust::waproto::whatsapp as wa;

/// The reply context on `message`, if it is a reply.
///
/// `message` must already be unwrapped to its base body; the ephemeral and
/// view-once envelopes carry no context of their own.
pub fn quoted_from(message: &wa::Message) -> Option<QuotedMessage> {
    let context = context_info(message)?;
    // A quote needs the original's id to be worth anything: without it there
    // is nothing to jump to and nothing to key the snapshot on.
    let message_id = context.stanza_id.clone()?;
    // The body is optional and the id is not, deliberately: a resend, or a
    // reply to something large, carries the linkage without the original.
    // Dropping the whole quote there left the reply drawn as an ordinary
    // message, with nothing to say it was a reply and nowhere to jump to.
    let base = context
        .quoted_message
        .as_option()
        .map(oxidezap_chat_store::normalized_message);

    let sender = context.participant.clone().unwrap_or_default();
    Some(QuotedMessage {
        message_id,
        // Push names for the quoted author are not in the envelope.
        // `Chat::name_quoted_author` fills this in when the message joins its
        // chat, from the participant map and the original message — which
        // know the current name rather than the one at quoting time.
        sender_name: String::new(),
        sender,
        preview: base
            .and_then(|base| {
                oxidezap_chat_store::business_content(base)
                    .map(|b| b.summary())
                    .or_else(|| oxidezap_chat_store::sticker_pack_text(base))
                    .or_else(|| {
                        base.text_content()
                            .or_else(|| base.get_caption())
                            .map(str::to_owned)
                    })
            })
            .unwrap_or_default(),
        kind: base.and_then(quoted_kind),
    })
}

/// The `ContextInfo` on whichever body carries it.
///
/// Ordered by how often a reply uses each: text first, then the media kinds.
fn context_info(message: &wa::Message) -> Option<&wa::ContextInfo> {
    macro_rules! first_context {
        ($($field:ident),+ $(,)?) => {{
            let mut found: Option<&wa::ContextInfo> = None;
            $(
                if found.is_none()
                    && let Some(body) = message.$field.as_option()
                    && let Some(context) = body.context_info.as_option()
                {
                    found = Some(context);
                }
            )+
            found
        }};
    }

    first_context!(
        extended_text_message,
        image_message,
        video_message,
        // A video note is a `VideoMessage` under its own field, and it is a
        // body a reply is really sent as. Left out, a reply recorded as one
        // lost its quote bar and its jump target while the same message's
        // *kind* was already read from here.
        ptv_message,
        audio_message,
        document_message,
        sticker_message,
        sticker_pack_message,
    )
    .or_else(|| {
        oxidezap_chat_store::business_contexts(message)
            .find(|context| context.stanza_id.is_some())
            .or_else(|| oxidezap_chat_store::business_contexts(message).next())
    })
}

fn quoted_kind(message: &wa::Message) -> Option<QuotedKind> {
    if let Some(media) = oxidezap_chat_store::business_media(message) {
        use oxidezap_chat_store::BusinessMedia;
        return match media {
            BusinessMedia::Image(_) | BusinessMedia::Thumbnail(_) => Some(QuotedKind::Image),
            BusinessMedia::Video(_) => Some(QuotedKind::Video),
            BusinessMedia::Document(_) => Some(QuotedKind::Document),
            BusinessMedia::Location(_) => None,
        };
    }
    if message.image_message.is_set() {
        Some(QuotedKind::Image)
    } else if message.video_message.is_set() || message.ptv_message.is_set() {
        Some(QuotedKind::Video)
    } else if message.audio_message.is_set() {
        Some(QuotedKind::Audio)
    } else if message.document_message.is_set() {
        Some(QuotedKind::Document)
    } else if message.sticker_message.is_set() {
        Some(QuotedKind::Sticker)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Re-exported by waproto because its types permeate the generated API;
    // depending on it directly would mean version-matching that crate exactly.
    use whatsapp_rust::waproto::buffa;
    use whatsapp_rust::waproto::whatsapp::message;

    fn text_reply(context: wa::ContextInfo) -> wa::Message {
        {
            let mut constructed: wa::Message = Default::default();
            constructed.extended_text_message = buffa::MessageField::some({
                let mut constructed: message::ExtendedTextMessage = Default::default();
                constructed.text = Some("e o áudio?".to_string());
                constructed.context_info = buffa::MessageField::some(context);
                constructed
            });
            constructed
        }
    }

    fn quoted_text(text: &str) -> buffa::MessageField<wa::Message> {
        buffa::MessageField::some({
            let mut constructed: wa::Message = Default::default();
            constructed.conversation = Some(text.to_string());
            constructed
        })
    }

    #[test]
    fn reads_a_text_reply() {
        let message = text_reply({
            let mut constructed: wa::ContextInfo = Default::default();
            constructed.stanza_id = Some("ORIGINAL".to_string());
            constructed.participant = Some("a@s.whatsapp.net".to_string());
            constructed.quoted_message = quoted_text("ping");
            constructed
        });
        let quoted = quoted_from(&message).expect("this is a reply");
        assert_eq!(quoted.message_id, "ORIGINAL");
        assert_eq!(quoted.sender, "a@s.whatsapp.net");
        assert_eq!(quoted.preview, "ping");
        assert_eq!(quoted.kind, None);
    }

    #[test]
    fn a_plain_message_is_not_a_reply() {
        let message = {
            let mut constructed: wa::Message = Default::default();
            constructed.conversation = Some("ping".to_string());
            constructed
        };
        assert!(quoted_from(&message).is_none());
    }

    #[test]
    fn context_without_a_stanza_id_yields_no_quote() {
        // Mentions also travel in ContextInfo; that is not a reply, and
        // rendering an empty quote bar for one would be worse than nothing.
        let message = text_reply({
            let mut constructed: wa::ContextInfo = Default::default();
            constructed.mentioned_jid = vec!["a@s.whatsapp.net".to_string()];
            constructed
        });
        assert!(quoted_from(&message).is_none());
    }

    /// A video note is a reply body like any other. `quoted_kind` already
    /// named it; the context lookup did not look at it.
    #[test]
    fn reads_a_reply_sent_as_a_video_note() {
        let message = {
            let mut constructed: wa::Message = Default::default();
            constructed.ptv_message = buffa::MessageField::some({
                let mut constructed: message::VideoMessage = Default::default();
                constructed.context_info = buffa::MessageField::some({
                    let mut constructed: wa::ContextInfo = Default::default();
                    constructed.stanza_id = Some("ORIGINAL".to_string());
                    constructed.participant = Some("a@s.whatsapp.net".to_string());
                    constructed.quoted_message = quoted_text("ping");
                    constructed
                });
                constructed
            });
            constructed
        };
        let quoted = quoted_from(&message).expect("a video note can be a reply");
        assert_eq!(quoted.message_id, "ORIGINAL");
        assert_eq!(quoted.preview, "ping");
    }

    /// A resend, or a reply to something large, carries the linkage without
    /// the quoted body. The jump target is in hand either way.
    #[test]
    fn a_reply_without_the_quoted_body_keeps_its_jump_target() {
        let message = text_reply({
            let mut constructed: wa::ContextInfo = Default::default();
            constructed.stanza_id = Some("ORIGINAL".to_string());
            constructed.participant = Some("a@s.whatsapp.net".to_string());
            constructed
        });
        let quoted = quoted_from(&message).expect("the linkage is the quote");
        assert_eq!(quoted.message_id, "ORIGINAL");
        assert_eq!(quoted.sender, "a@s.whatsapp.net");
        assert_eq!(quoted.preview, "");
        assert_eq!(quoted.kind, None);
    }

    #[test]
    fn a_quoted_photo_reports_its_kind() {
        let message = text_reply({
            let mut constructed: wa::ContextInfo = Default::default();
            constructed.stanza_id = Some("ORIGINAL".to_string());
            constructed.participant = Some("a@s.whatsapp.net".to_string());
            constructed.quoted_message = buffa::MessageField::some({
                let mut constructed: wa::Message = Default::default();
                constructed.image_message =
                    buffa::MessageField::some(message::ImageMessage::default());
                constructed
            });
            constructed
        });
        let quoted = quoted_from(&message).unwrap();
        assert_eq!(quoted.kind, Some(QuotedKind::Image));
        assert_eq!(quoted.summary(), "Photo");
    }
}
