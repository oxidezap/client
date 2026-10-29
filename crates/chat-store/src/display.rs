//! Display-only projections for content without a dedicated interactive renderer.
use crate::MessageKind;
use waproto::whatsapp as wa;

fn bounded(value: Option<&str>) -> String {
    let value = value.unwrap_or_default();
    let mut end = value.len().min(4096);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = value[..end].to_owned();
    if end < value.len() {
        out.push('…');
    }
    out
}

/// A pack is an archive of stickers, not a WebP image. Display its public
/// metadata without interpreting archive paths, URLs or sticker-pack IDs.
pub fn sticker_pack_text(message: &wa::Message) -> Option<String> {
    let pack = crate::normalized_message(message)
        .sticker_pack_message
        .as_option()?;
    let mut lines = vec!["Sticker pack".to_owned()];
    lines.extend(
        [
            pack.name.as_deref(),
            pack.publisher.as_deref(),
            pack.pack_description.as_deref(),
            pack.caption.as_deref(),
        ]
        .into_iter()
        .map(bounded)
        .filter(|s| !s.trim().is_empty()),
    );
    lines.push("Open WhatsApp on your phone to view or add this pack.".into());
    Some(lines.join("\n"))
}

/// The same honest fallback on live reception and after reloading. Unknown
/// future content stays in storage and never becomes debug payload text.
pub fn message_fallback(kind: &MessageKind) -> &'static str {
    match kind {
        MessageKind::Sticker => "Sticker unavailable",
        MessageKind::StickerPack => "Sticker pack unavailable. Open it in WhatsApp on your phone.",
        MessageKind::Image => "Photo unavailable",
        MessageKind::Video | MessageKind::VideoNote => "Video unavailable",
        MessageKind::Audio | MessageKind::VoiceNote => "Audio unavailable",
        MessageKind::Document => "Document unavailable",
        MessageKind::Poll => "This poll is not supported yet. Open it in WhatsApp on your phone.",
        MessageKind::Album => "Media album. Open it in WhatsApp on your phone.",
        MessageKind::Product => "Product. Open it in WhatsApp on your phone.",
        MessageKind::Location => "Location. Open it in WhatsApp on your phone.",
        MessageKind::Event => "Event. Open it in WhatsApp on your phone.",
        MessageKind::GroupInvite => "Group invitation. Open it in WhatsApp on your phone.",
        MessageKind::Undecryptable => "Waiting for this message. This may take a while.",
        MessageKind::ViewOnce => "View-once message. Open it in WhatsApp on your phone.",
        MessageKind::Hosted | MessageKind::Bot => {
            "This message is only available in WhatsApp on your phone."
        }
        _ => "This message is not supported yet. Open it in WhatsApp on your phone.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffa::MessageField;

    #[test]
    fn pack_metadata_is_bounded_and_does_not_expose_archive_names_or_ids() {
        let mut pack = wa::message::StickerPackMessage::default();
        pack.name = Some("Example pack".into());
        pack.publisher = Some("Example artist".into());
        pack.caption = Some("A caption".into());
        pack.pack_description = Some("é".repeat(5000));
        pack.sticker_pack_id = Some("private-identifier".into());
        pack.tray_icon_file_name = Some("archive-path".into());
        let mut message = wa::Message::default();
        message.sticker_pack_message = MessageField::some(pack);
        let projected = sticker_pack_text(&message).unwrap();
        assert!(projected.contains("Example pack\nExample artist"));
        assert!(projected.contains("A caption"));
        assert!(!projected.contains("private-identifier"));
        assert!(!projected.contains("archive-path"));
        assert!(projected.len() < 5000);
        assert_eq!(crate::message_kind(&message), "sticker_pack");
    }

    #[test]
    fn future_kinds_do_not_expose_internal_labels() {
        assert_eq!(
            message_fallback(&MessageKind::Other("internal-future-kind".into())),
            message_fallback(&MessageKind::Unknown)
        );
        assert!(sticker_pack_text(&wa::Message::default()).is_none());
    }
}
