//! Where the media bytes a frame carries go.
//!
//! Nothing on the wire carries them: `MediaContent::data` is skipped by serde
//! wherever it travels, so every frame that carries a `ChatMessage` passes
//! through here to leave a cache key behind instead.

use oxidezap_core::{ChatMessage, MediaContent, UiEvent};

use crate::media::AccountMedia;
use crate::state::StateHub;

/// Move an event's media bytes into the cache and leave a key behind.
///
/// The bytes stay where they were in this process — `data` is skipped by
/// serde, so the frame carries the key alone. A front end reads the file once
/// and decodes it into the image cache it already keeps.
///
/// Writing is skipped for anything already cached, which is most of it after
/// the first attach: a message's media is addressed by its message id, and a
/// message's media does not change.
pub(crate) fn externalize_media(hub: &StateHub, event: &mut UiEvent) {
    // Read once for the whole event: this runs on the publish thread behind
    // an unbounded queue, so a clear can land between being handed the event
    // and writing its media. See `media::put_since`. This account's own epoch,
    // so another account's clear does not refuse this write.
    let media = AccountMedia::new(hub.account_id());
    let epoch = media.epoch();
    match event {
        UiEvent::MessageReceived { message, .. } => {
            cache_media(&media, epoch, &message.id, &mut message.media)
        }
        UiEvent::HistoryLoaded { chats, .. } => {
            for chat in chats {
                externalize_messages(&media, epoch, &mut chat.messages);
            }
        }
        _ => {}
    }
}

/// The same, for a page this daemon was asked for.
///
/// A page of history reaches a front end without passing through the event
/// stream, and `MediaContent::data` is skipped by serde wherever it travels —
/// so a page serialized straight out of the store carries neither the bytes
/// nor a key to find them by, and older photos draw as download-only next to
/// the identical rows the attach load externalized. Every frame that carries
/// a `ChatMessage` goes through here.
pub(super) fn externalize_messages(
    media: &AccountMedia,
    epoch: usize,
    messages: &mut [ChatMessage],
) {
    for message in messages {
        let id = message.id.clone();
        cache_media(media, epoch, &id, &mut message.media);
    }
}

fn cache_media(
    account_media: &AccountMedia,
    cache_epoch: usize,
    message_id: &str,
    media: &mut Option<MediaContent>,
) {
    let Some(media) = media else { return };
    let local_key = local_media_key(message_id, media);
    let key = account_media.key(&local_key);

    // Only the real thing is cached. A fallback thumbnail written under the
    // message's key would take the place of the full image already there —
    // and a hydrated row carries a thumbnail every time, so the cache would
    // lose a photo to a blur on the first reload after seeing it.
    let is_cacheable = !media.data.is_empty() && !media.data_is_preview;
    if !is_cacheable {
        // Nothing to write, but the bytes may already be here: the store
        // never holds media, so this is what makes a photo survive a restart
        // instead of being downloaded again.
        if account_media.has(&key) {
            media.cache_key = Some(key);
            return;
        }
        // The other key the same bytes can be under. A download is cached by
        // its content — `d-<hash>` — and only the eager fetch writes the
        // message's own key, so a photo whose eager fetch failed and was
        // fetched on demand later is on this disk under a name a hydrated row
        // never looks for. It was downloaded again on every restart.
        if let Some(downloadable) = &media.downloadable
            && let Some(by_content) = account_media.download_key(downloadable.content_hash())
            && account_media.has(&by_content)
        {
            media.cache_key = Some(by_content);
        }
        return;
    }

    // Nobody asked for this one: it is the eager cache of media that arrived
    // with a message, and the front end can fetch it on demand if it is not
    // here. So a clear that lands while it is queued wins, and the directory
    // the user just emptied stays empty.
    match account_media.put_since(cache_epoch, &local_key, &media.data) {
        Ok(key) => media.cache_key = Some(key),
        // The front end still gets the message; the media renders as the
        // download it also is. A cache that cannot be written is not a reason
        // to drop a conversation.
        Err(e) => log::warn!("could not cache media for a message: {e}"),
    }
}

// Previous clients cached channel thumbnails as full images under f-<message>.
// Plaintext identity bypasses those entries while preserving E2EE cache keys.
fn local_media_key(message_id: &str, media: &MediaContent) -> String {
    media
        .downloadable
        .as_ref()
        .filter(|d| d.media_key.is_empty())
        .and_then(|d| crate::media::download_key(d.content_hash()))
        .unwrap_or_else(|| crate::media::message_key(message_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_core::DownloadableMedia;

    #[test]
    fn channel_previews_bypass_old_message_thumbnail_cache_entries() {
        let mut media =
            MediaContent::image(std::sync::Arc::new(vec![1]), "image/jpeg".into(), true);
        let dl: DownloadableMedia = serde_json::from_value(serde_json::json!({
            "direct_path":"/v/channel", "media_key":[], "file_enc_sha256":[],
            "file_sha256":vec![7;32], "file_length":1024, "mime_type":"image/jpeg",
            "duration_secs":null, "download_type":"image"
        }))
        .unwrap();
        media.downloadable = Some(dl);
        let local = local_media_key("channel-image", &media);
        assert_eq!(local, crate::media::download_key(&[7; 32]).unwrap());
        assert_ne!(local, crate::media::message_key("channel-image"));
        // Completing a download must keep the same identity for reloads.
        media.adopt_full_bytes(std::sync::Arc::new(vec![2, 3]));
        assert_eq!(local_media_key("channel-image", &media), local);
        let dl = media.downloadable.as_mut().unwrap();
        dl.media_key = vec![1; 32];
        dl.file_enc_sha256 = vec![2; 32];
        assert_eq!(
            local_media_key("encrypted-image", &media),
            crate::media::message_key("encrypted-image")
        );
    }
}
