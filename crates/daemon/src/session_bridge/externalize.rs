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
    media.cache_key = None;
    media.preview_cache_key = None;
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
            return;
        }
        if let Some(thumbnail) = media.preview_downloadable.as_ref()
            && let Some(key) = account_media.download_key(thumbnail.content_hash())
            && account_media.has(&key)
        {
            // No inline JPEG is required for a cached server preview. Clear
            // old inline bytes before labeling the cache payload as HQ.
            media.adopt_server_preview(std::sync::Arc::new(Vec::new()));
            media.preview_cache_key = Some(key);
            return;
        }
        // Inline bytes also cross the process boundary, under an identity
        // that can never satisfy a full-media lookup. Oversized inline data
        // stays a download placeholder rather than consuming a frame budget.
        if media.data_is_preview && !media.data.is_empty() && media.data.len() <= 256 * 1024 {
            let preview_key = format!("d-preview-{local_key}");
            match account_media.put_since(cache_epoch, &preview_key, &media.data) {
                Ok(key) => media.preview_cache_key = Some(key),
                Err(e) => log::warn!("could not transfer inline media preview: {e}"),
            }
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
    fn an_inline_channel_fallback_crosses_the_wire_without_poisoning_the_original() {
        let cache = AccountMedia::new(oxidezap_core::AccountId::new(731).unwrap());
        let mut hash = vec![0x7b; 32];
        hash[..4].copy_from_slice(&std::process::id().to_be_bytes());
        let dl: DownloadableMedia = serde_json::from_value(serde_json::json!({
            "direct_path":"/v/channel-preview", "media_key":[], "file_enc_sha256":[],
            "file_sha256":hash, "file_length":1024, "mime_type":"image/webp",
            "duration_secs":null, "download_type":"image"
        }))
        .unwrap();
        let mut original =
            MediaContent::image(std::sync::Arc::new(vec![1, 2]), "image/jpeg".into(), true)
                .with_download(Some(dl));
        let mut thumbnail = original.downloadable.clone().unwrap();
        thumbnail.file_sha256 = vec![0x91; 32];
        thumbnail.is_thumbnail = true;
        original.preview_downloadable = Some(thumbnail.clone());
        let full_local = local_media_key("42", &original);
        let full_key = cache.key(&full_local);
        let mut fallback = Some(original.clone());
        cache_media(&cache, cache.epoch(), "42", &mut fallback);
        let sent = fallback.as_ref().unwrap();
        let preview_key = sent.preview_cache_key.as_ref().expect("preview transfer");
        assert!(cache.has(preview_key));
        assert!(!cache.has(&full_key), "thumbnail is never the original");
        assert!(sent.cache_key.is_none());
        let decoded: MediaContent =
            serde_json::from_str(&serde_json::to_string(sent).unwrap()).unwrap();
        assert!(decoded.data.is_empty());
        assert!(decoded.data_is_preview);
        assert_eq!(decoded.preview_cache_key.as_ref(), Some(preview_key));
        let hq_local = crate::media::download_key(thumbnail.content_hash()).unwrap();
        let hq_key = cache.put_owned(&hq_local, vec![6, 5, 4]).unwrap();
        let mut hq_hydrated = Some(original.clone());
        cache_media(&cache, cache.epoch(), "42", &mut hq_hydrated);
        assert_eq!(
            hq_hydrated.unwrap().preview_cache_key.as_ref(),
            Some(&hq_key)
        );
        let mut without_inline = original.clone();
        without_inline.data = std::sync::Arc::new(Vec::new());
        without_inline.data_is_preview = false;
        without_inline.mime_type = "image/webp".into();
        let mut without_inline = Some(without_inline);
        cache_media(&cache, cache.epoch(), "42", &mut without_inline);
        let hq = without_inline.unwrap();
        assert!(hq.data_is_preview && hq.data_is_server_preview);
        assert_eq!(hq.mime_type, "image/jpeg");
        assert_eq!(hq.preview_cache_key.as_ref(), Some(&hq_key));
        assert_eq!(
            hq.downloadable, original.downloadable,
            "original stays available"
        );

        cache.put_owned(&full_local, vec![9, 8, 7]).unwrap();
        let mut hydrated = Some(original);
        cache_media(&cache, cache.epoch(), "42", &mut hydrated);
        let full = hydrated.unwrap();
        assert_eq!(full.cache_key.as_ref(), Some(&full_key));
        assert!(
            full.preview_cache_key.is_none(),
            "cached original takes precedence"
        );
    }

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
