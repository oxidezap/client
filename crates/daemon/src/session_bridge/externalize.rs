//! Where the media bytes a frame carries go.
//!
//! Nothing on the wire carries them: `MediaContent::data` is skipped by serde
//! wherever it travels, so every frame that carries a `ChatMessage` passes
//! through here to leave a cache key behind instead.

use oxidezap_core::{ChatMessage, MediaContent, UiEvent};

/// Move an event's media bytes into the cache and leave a key behind.
///
/// The bytes stay where they were in this process — `data` is skipped by
/// serde, so the frame carries the key alone. A front end reads the file once
/// and decodes it into the image cache it already keeps.
///
/// Writing is skipped for anything already cached, which is most of it after
/// the first attach: a message's media is addressed by its message id, and a
/// message's media does not change.
pub(crate) fn externalize_media(event: &mut UiEvent) {
    // Read once for the whole event: this runs on the publish thread behind
    // an unbounded queue, so a clear can land between being handed the event
    // and writing its media. See `media::put_since`.
    let epoch = crate::media::epoch();
    match event {
        UiEvent::MessageReceived { message, .. } => {
            cache_media(epoch, &message.id, &mut message.media)
        }
        UiEvent::HistoryLoaded { chats, .. } => {
            for chat in chats {
                externalize_messages(epoch, &mut chat.messages);
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
pub(super) fn externalize_messages(epoch: usize, messages: &mut [ChatMessage]) {
    for message in messages {
        let id = message.id.clone();
        cache_media(epoch, &id, &mut message.media);
    }
}

fn cache_media(cache_epoch: usize, message_id: &str, media: &mut Option<MediaContent>) {
    let Some(media) = media else { return };
    let key = crate::media::message_key(message_id);

    // Only the real thing is cached. A fallback thumbnail written under the
    // message's key would take the place of the full image already there —
    // and a hydrated row carries a thumbnail every time, so the cache would
    // lose a photo to a blur on the first reload after seeing it.
    let is_cacheable = !media.data.is_empty() && !media.data_is_preview;
    if !is_cacheable {
        // Nothing to write, but the bytes may already be here: the store
        // never holds media, so this is what makes a photo survive a restart
        // instead of being downloaded again.
        if crate::media::has(&key) {
            media.cache_key = Some(key);
            return;
        }
        // The other key the same bytes can be under. A download is cached by
        // its content — `d-<hash>` — and only the eager fetch writes the
        // message's own key, so a photo whose eager fetch failed and was
        // fetched on demand later is on this disk under a name a hydrated row
        // never looks for. It was downloaded again on every restart.
        if let Some(downloadable) = &media.downloadable
            && let Some(by_content) = crate::media::download_key(&downloadable.file_enc_sha256)
            && crate::media::has(&by_content)
        {
            media.cache_key = Some(by_content);
        }
        return;
    }

    // Nobody asked for this one: it is the eager cache of media that arrived
    // with a message, and the front end can fetch it on demand if it is not
    // here. So a clear that lands while it is queued wins, and the directory
    // the user just emptied stays empty.
    match crate::media::put_since(cache_epoch, &key, &media.data) {
        Ok(key) => media.cache_key = Some(key),
        // The front end still gets the message; the media renders as the
        // download it also is. A cache that cannot be written is not a reason
        // to drop a conversation.
        Err(e) => log::warn!("could not cache media for a message: {e}"),
    }
}
