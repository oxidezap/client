//! Naming the people a message `@`-mentions.
//!
//! The text carries `@` plus the address's user part; the mention list beside
//! it carries the full JIDs. Rewriting the one with the other's names is what
//! stops a bubble reading `@111222333444555`: the digits of a LID, which no
//! reader can dial and no contact is known by.
//!
//! Resolution is the same order every other label uses — the address book,
//! then the number — so a mention reads "Ana" over bubbles from "Ana" rather
//! than naming a second person. A mention is baked into the text it stands
//! in, which is why the answer is written onto the content rather than kept
//! beside it the way `sender_name` is: there is nowhere later for it to
//! arrive from.

use std::sync::Arc;

use oxidezap_chat_store::StoredMessage;
use oxidezap_core::ChatMessage;
use whatsapp_rust::client::Client;
use whatsapp_rust::wacore::proto_helpers::MessageExt;
use whatsapp_rust::wacore_binary::jid::Jid;
use whatsapp_rust::waproto::whatsapp as wa;

use crate::names::NameBook;

/// Read the mention lists off stored rows, in order.
///
/// Split from [`hydrate_mention_lists`] because the conversion between them
/// consumes the rows the lists are read off.
pub(crate) fn mention_lists_of(stored: &[StoredMessage]) -> Vec<Vec<String>> {
    stored
        .iter()
        .map(|row| mentioned_jids(row.message.as_deref()))
        .collect()
}

/// Rewrite the `@`-mentions in hydrated rows to the names the book has.
///
/// Stored text is the wire's verbatim `@`-plus-digits; the mention lists read
/// off the protos before conversion say which digits are mentions. The live
/// path rewrites before publishing while the store keeps the verbatim text,
/// so without this every reloaded bubble shows the digits the live one had
/// just replaced. `mention_lists` runs parallel to `msgs`.
pub(crate) async fn hydrate_mention_lists(
    client: &Arc<Client>,
    names: &NameBook,
    mention_lists: &[Vec<String>],
    msgs: &mut [ChatMessage],
) {
    for (jids, msg) in mention_lists.iter().zip(msgs.iter_mut()) {
        if jids.is_empty() {
            continue;
        }
        let pairs = mention_names(client, names, jids).await;
        if pairs.is_empty() {
            continue;
        }
        msg.content = apply_to(&pairs, std::mem::take(&mut msg.content));
        if let Some(media) = msg.media.as_mut()
            && let Some(caption) = media.caption.take()
        {
            media.caption = Some(apply_to(&pairs, caption));
        }
    }
}

/// The JIDs `message` mentions, as written in its mention list.
///
/// Read off the unwrapped body, whatever kind it is — a mention can stand in
/// a caption as well as in the text — so text and media captions share this.
/// Empty when the message is absent or names nobody, which is most messages.
pub(crate) fn mentioned_jids(message: Option<&wa::Message>) -> Vec<String> {
    let Some(message) = message else {
        return Vec::new();
    };
    let base = peel(message);
    let mut jids = Vec::new();
    macro_rules! collect {
        ($($field:ident),+ $(,)?) => {$(
            if let Some(body) = base.$field.as_option()
                && let Some(context) = body.context_info.as_option()
            {
                jids.extend(context.mentioned_jid.iter().cloned());
            }
        )+};
    }
    // The bodies a mention is ever sent as: text first, then the captioned
    // media kinds. The same set a reply is read off in `crate::quoting`.
    collect!(
        extended_text_message,
        image_message,
        video_message,
        ptv_message,
        audio_message,
        document_message,
        sticker_message,
    );
    jids.sort();
    jids.dedup();
    jids
}

/// Rewrite the `@`-mentions in `text` to the names the book has for them.
///
/// `message` is the proto the text was read off, or `None` where no proto
/// survived — without the mention list there is nothing to rename by, and the
/// text stands as typed. Every JID the list names gets the book's answer or
/// the same last resort every other surface falls back to, so a stranger
/// reads in a mention the way they read over their own bubble.
///
/// Most messages mention nobody, and those cost nothing here: the list is
/// read synchronously first, and the book is only asked when it names
/// someone.
pub(crate) async fn resolve_for(
    client: &Arc<Client>,
    names: &NameBook,
    message: Option<&wa::Message>,
) -> Vec<(String, String)> {
    let jids = mentioned_jids(message);
    if jids.is_empty() {
        return Vec::new();
    }
    mention_names(client, names, &jids).await
}

/// Apply resolved `(user part, display name)` pairs to `text.
///
/// Split from [`resolve_for`] because a message carries two texts that can
/// mention someone — the body and a media caption — and both are drawn from
/// the same pairs.
pub(crate) fn apply_to(pairs: &[(String, String)], text: String) -> String {
    if pairs.is_empty() {
        return text;
    }
    let refs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(user, name)| (user.as_str(), name.as_str()))
        .collect();
    oxidezap_core::format_mentions(&text, &refs)
}

/// What each mentioned JID is called, as `(user part, display name)` pairs.
///
/// The book's answer where it has one, the number where the address carries
/// one, and the generic label otherwise. A JID that will not even parse names
/// nothing and is skipped: its token stays as typed rather than gaining a
/// name guessed from digits.
async fn mention_names(
    client: &Arc<Client>,
    names: &NameBook,
    jids: &[String],
) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(jids.len());
    for jid in jids {
        let Ok(parsed) = jid.parse::<Jid>() else {
            continue;
        };
        let identity = names.identity(client, &parsed).await;
        let name = names
            .known(client, &parsed, None)
            .await
            .unwrap_or_else(|| identity.fallback_name.clone());
        let user = parsed.user_base().to_string();
        if out.iter().all(|(known, _)| *known != user) {
            out.push((user, name));
        }
    }
    out
}

/// The message under its envelopes.
///
/// The same wrappers the store peels before classifying a row: a mention
/// inside one is still a mention, and the mention list lives on the inner
/// body's context rather than the envelope's.
fn peel(message: &wa::Message) -> &wa::Message {
    for wrapper in [
        &message.group_mentioned_message,
        &message.associated_child_message,
        &message.poll_creation_message_v4,
    ] {
        if let Some(inner) = wrapper.as_option().and_then(|w| w.message.as_option()) {
            return inner.get_base_message();
        }
    }
    message.get_base_message()
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatsapp_rust::waproto::buffa;
    use whatsapp_rust::waproto::whatsapp::message;

    fn text_message(text: &str, mentioned: &[&str]) -> wa::Message {
        wa::Message {
            extended_text_message: buffa::MessageField::some(message::ExtendedTextMessage {
                text: Some(text.to_string()),
                context_info: buffa::MessageField::some(wa::ContextInfo {
                    mentioned_jid: mentioned.iter().map(ToString::to_string).collect(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn no_proto_names_nothing() {
        assert!(mentioned_jids(None).is_empty());
    }

    #[test]
    fn a_text_mention_lists_its_jids() {
        let message = text_message("oi @559900000002", &["559900000002@s.whatsapp.net"]);
        assert_eq!(
            mentioned_jids(Some(&message)),
            vec!["559900000002@s.whatsapp.net".to_string()]
        );
    }

    #[test]
    fn a_caption_mention_lists_its_jids() {
        let message = wa::Message {
            image_message: buffa::MessageField::some(message::ImageMessage {
                caption: Some("olha @559900000002".to_string()),
                context_info: buffa::MessageField::some(wa::ContextInfo {
                    mentioned_jid: vec!["559900000002@s.whatsapp.net".to_string()],
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            mentioned_jids(Some(&message)),
            vec!["559900000002@s.whatsapp.net".to_string()]
        );
    }

    #[test]
    fn a_message_naming_nobody_lists_nothing() {
        let message = wa::Message {
            conversation: Some("bom dia".to_string()),
            ..Default::default()
        };
        assert!(mentioned_jids(Some(&message)).is_empty());
    }

    #[test]
    fn repeated_jids_are_listed_once() {
        let message = text_message("oi @1 e @1", &["1@s.whatsapp.net", "1@s.whatsapp.net"]);
        assert_eq!(
            mentioned_jids(Some(&message)),
            vec!["1@s.whatsapp.net".to_string()]
        );
    }

    #[test]
    fn a_mention_inside_a_wrapper_is_still_listed() {
        let inner = text_message("oi @559900000002", &["559900000002@s.whatsapp.net"]);
        let message = wa::Message {
            group_mentioned_message: buffa::MessageField::some(message::FutureProofMessage {
                message: buffa::MessageField::some(inner),
            }),
            ..Default::default()
        };
        assert_eq!(
            mentioned_jids(Some(&message)),
            vec!["559900000002@s.whatsapp.net".to_string()]
        );
    }
}
