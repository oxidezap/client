//! What a message's media is, read once.
//!
//! A message reaches this side twice — live off the socket, and again out of
//! the store when a conversation is hydrated — and both roads want the same
//! answer: which of the five media kinds this message carries, what bytes are
//! in hand for it now, and what it takes to fetch the rest later. The two used
//! to spell that answer out separately, five branches each, and they drifted:
//! a field defaulted differently on one side is a bubble that changes when a
//! chat is reloaded.
//!
//! So the answer is [`media_of`], and the *only* thing the live path adds is
//! bytes it already fetched. [`media_now`] is that path: it decides whether an
//! eager download is worth making, makes it, and hands the result to the same
//! function hydration calls with `None`.

use std::sync::Arc;

use log::{info, warn};
use oxidezap_core::{DownloadableMedia, MediaContent};
use whatsapp_rust::client::Client;
use whatsapp_rust::wacore::download::Downloadable;
use whatsapp_rust::waproto::whatsapp as wa;

/// Most bytes a picture may be worth fetching before anybody has asked
/// for it.
///
/// A photo sent through WhatsApp is a fraction of this; past it the
/// message keeps its thumbnail and its download metadata, which is what
/// the renderer already draws for a video, and the full bytes arrive when
/// somebody opens it.
pub(super) const EAGER_MEDIA_BYTES: u64 = 4 * 1024 * 1024;

/// Whether media of this size is worth fetching before anybody asked.
pub(super) fn worth_fetching_now(eager: bool, file_length: Option<u64>) -> bool {
    eager && file_length.is_none_or(|len| len <= EAGER_MEDIA_BYTES)
}

/// The eager fetch, or nothing when this is not the moment for one.
async fn fetch_now<T: Downloadable>(
    client: &Arc<Client>,
    media: &T,
    media_name: &str,
    eager: bool,
    file_length: Option<u64>,
) -> Option<Vec<u8>> {
    if !worth_fetching_now(eager, file_length) {
        return None;
    }
    download_media(client, media, media_name).await
}

/// Helper to download media with logging
async fn download_media<T: Downloadable>(
    client: &Arc<Client>,
    media: &T,
    media_name: &str,
) -> Option<Vec<u8>> {
    info!("Downloading {}...", media_name);
    let limits = if !media.is_encrypted()
        && media.app_info() == whatsapp_rust::wacore::download::MediaType::Image
    {
        crate::net::channel_image_limits()
    } else {
        whatsapp_rust::download::DownloadLimits::default()
    };
    match client.download(media, limits).await {
        Ok(data) => {
            info!(
                "{} downloaded successfully: {} bytes",
                media_name,
                data.len()
            );
            Some(data)
        }
        Err(e) => {
            warn!("Failed to download {}: {}", media_name, e);
            None
        }
    }
}

/// Some animated stickers arrive wrapped in the `lottie_sticker_message`
/// future-proof envelope instead of the top-level `sticker_message`.
fn effective_sticker(msg: &wa::Message) -> Option<&wa::message::StickerMessage> {
    msg.sticker_message.as_option().or_else(|| {
        msg.lottie_sticker_message
            .as_option()
            .and_then(|w| w.message.as_option())
            .and_then(|m| m.sticker_message.as_option())
    })
}

fn image_of(msg: &wa::Message) -> Option<&wa::message::ImageMessage> {
    msg.image_message
        .as_option()
        .or_else(|| match oxidezap_chat_store::business_media(msg) {
            Some(oxidezap_chat_store::BusinessMedia::Image(m)) => Some(m),
            _ => None,
        })
}
fn video_of(msg: &wa::Message) -> Option<&wa::message::VideoMessage> {
    msg.ptv_message
        .as_option()
        .or(msg.video_message.as_option())
        .or_else(|| match oxidezap_chat_store::business_media(msg) {
            Some(oxidezap_chat_store::BusinessMedia::Video(m)) => Some(m),
            _ => None,
        })
}
fn document_of(msg: &wa::Message) -> Option<&wa::message::DocumentMessage> {
    msg.document_message
        .as_option()
        .or_else(|| match oxidezap_chat_store::business_media(msg) {
            Some(oxidezap_chat_store::BusinessMedia::Document(m)) => Some(m),
            _ => None,
        })
}

/// Same HTTPS/CDN boundary as the preserved Web media URL allowlist.
fn is_media_cdn_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or_default();
    host.bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.')
        && (host == "whatsapp.net" || host.ends_with(".whatsapp.net"))
}

/// What it takes to fetch this media later, or `None` when the message did
/// not carry enough to try.
///
/// Every field but the mime type and the duration is already spelled out by
/// the library's own [`Downloadable`], which each of the five message protos
/// implements — including `app_info`, whose type *is* our `download_type`. So
/// the five kinds do not each need their own copy of this; asking the trait is
/// what keeps a proto that grows a field from having to be found in five
/// places. The mime and the duration stay arguments because the kind's default
/// mime is the caller's business and only audio and video have a duration.
fn downloadable_of<T: Downloadable>(
    media: &T,
    mime_type: &str,
    duration_secs: Option<u32>,
) -> Option<DownloadableMedia> {
    // Channels carry plaintext media. Keep its integrity hash and static URL
    // instead of requiring E2EE fields that are absent from these messages.
    let direct_path = media.direct_path().unwrap_or_default();
    let static_url = media.static_url().filter(|url| is_media_cdn_url(url));
    if direct_path.is_empty() && static_url.is_none() {
        return None;
    }
    let media_key = media.media_key().unwrap_or_default();
    let file_enc_sha256 = media.file_enc_sha256().unwrap_or_default();
    let file_sha256 = media.file_sha256().unwrap_or_default();
    if media_key.is_empty() {
        if file_sha256.len() != 32 {
            return None;
        }
    } else if media_key.len() != 32 || file_enc_sha256.len() != 32 {
        return None;
    }
    Some(DownloadableMedia {
        direct_path: direct_path.to_string(),
        media_key: media_key.to_vec(),
        file_enc_sha256: file_enc_sha256.to_vec(),
        file_sha256: file_sha256.to_vec(),
        static_url: static_url.map(str::to_string),
        file_length: media.file_length().unwrap_or(0),
        mime_type: mime_type.to_string(),
        duration_secs,
        download_type: media.app_info(),
    })
}

/// The stand-in bytes a still carries before its media is fetched.
pub(super) struct Still {
    pub(super) data: Vec<u8>,
    pub(super) mime: String,
    pub(super) is_preview: bool,
}

fn thumbnail_bytes(thumbnail: Option<&[u8]>) -> Vec<u8> {
    thumbnail
        .filter(|t| !t.is_empty())
        .unwrap_or_default()
        .to_vec()
}

/// What a still is holding, decided once for the live path and the hydrated
/// one. They had drifted: the live path flagged a thumbnail as a preview with
/// no download metadata to make good on it, so the viewer refused to open the
/// only bytes that will ever exist and the daemon refused to cache them.
///
/// A preview is bytes standing in for a fetch that can actually be made, and
/// the mime describes what is in hand rather than what is being waited for.
/// The video paths do not come through here on purpose: a poster frame is
/// never the video, download metadata or not.
pub(super) fn still_preview(
    thumbnail: Vec<u8>,
    thumbnail_mime: &str,
    own_mime: String,
    downloadable: bool,
) -> Still {
    let has_preview = !thumbnail.is_empty();
    Still {
        mime: if has_preview {
            thumbnail_mime.to_string()
        } else {
            own_mime
        },
        is_preview: has_preview && downloadable,
        data: thumbnail,
    }
}

/// The media on a message, with `fetched` holding the full bytes when
/// somebody already had a reason to go and get them.
///
/// `fetched` answers the sticker and the image only, because they are the only
/// kinds ever fetched before they are asked for — see [`media_now`], which is
/// what produces it. A video's `data` is its poster frame whoever is calling;
/// an audio's and a document's is empty until the player or the save asks.
///
/// Not having fetched the bytes is the same shape as having failed to: the
/// thumbnail is what shows and the download metadata is what makes the full
/// bytes retryable.
pub(super) fn media_of(msg: &wa::Message, fetched: Option<Vec<u8>>) -> Option<MediaContent> {
    if let Some(oxidezap_chat_store::BusinessMedia::Thumbnail(bytes)) =
        oxidezap_chat_store::business_media(msg)
    {
        return (!bytes.is_empty())
            .then(|| MediaContent::image(Arc::new(bytes.to_vec()), "image/jpeg".into(), false));
    }
    if let Some(sticker) = effective_sticker(msg) {
        let mime = sticker
            .mimetype
            .clone()
            .unwrap_or_else(|| "image/webp".to_string());
        let downloadable = downloadable_of(sticker, &mime, None);
        // A failed (or skipped) eager download degrades to the thumbnail and
        // stays retryable through the download metadata, instead of the
        // message losing its media.
        let still = match fetched {
            Some(data) => Still {
                data,
                mime,
                is_preview: false,
            },
            None => still_preview(
                thumbnail_bytes(sticker.png_thumbnail.as_deref()),
                "image/png",
                mime,
                downloadable.is_some(),
            ),
        };
        if still.data.is_empty() && downloadable.is_none() {
            return None;
        }
        // `is_animated` is what the sticker *is*, not what the stand-in bytes
        // are; the constructor's own doc is where that reasoning lives.
        return Some(
            MediaContent::sticker(
                Arc::new(still.data),
                still.mime,
                still.is_preview,
                sticker.is_animated.unwrap_or(false),
            )
            .with_size(sticker.width, sticker.height)
            .with_download(downloadable),
        );
    }

    if let Some(image) = image_of(msg) {
        let mime = image
            .mimetype
            .clone()
            .unwrap_or_else(|| "image/jpeg".to_string());
        let downloadable = downloadable_of(image, &mime, None);
        // The same rule as the sticker: the thumbnail shows now and the full
        // image stays retryable, instead of the message degrading to a plain
        // text row for the whole session.
        let still = match fetched {
            Some(data) => Still {
                data,
                mime,
                is_preview: false,
            },
            None => still_preview(
                thumbnail_bytes(image.jpeg_thumbnail.as_deref()),
                "image/jpeg",
                mime,
                downloadable.is_some(),
            ),
        };
        if still.data.is_empty() && downloadable.is_none() {
            return None;
        }
        return Some(
            MediaContent::image(Arc::new(still.data), still.mime, still.is_preview)
                .with_size(image.width, image.height)
                .with_caption(image.caption.clone())
                .with_download(downloadable),
        );
    }

    // PTVs (round video notes) are the same proto type in a different field
    // and play like any other video.
    if let Some(video) = video_of(msg) {
        let downloadable = downloadable_of(
            video,
            video.mimetype.as_deref().unwrap_or("video/mp4"),
            video.seconds,
        );
        let thumbnail = thumbnail_bytes(video.jpeg_thumbnail.as_deref());
        if thumbnail.is_empty() && downloadable.is_none() {
            return None;
        }
        // A video's `data` is never the video: these are the JPEG bytes of its
        // poster frame, which is why the constructor takes it under that name
        // and settles the mime type and the preview flag itself.
        return Some(
            MediaContent::video(Arc::new(thumbnail), video.seconds)
                .with_size(video.width, video.height)
                .with_caption(video.caption.clone())
                .with_download(downloadable),
        );
    }

    // Audio is lazy either way: the bytes are fetched when somebody presses
    // play, so a voice note with no way to fetch them is nothing to draw.
    if let Some(audio) = msg.audio_message.as_option() {
        let mime = audio
            .mimetype
            .clone()
            .unwrap_or_else(|| "audio/ogg; codecs=opus".to_string());
        let downloadable = downloadable_of(audio, &mime, audio.seconds)?;
        // The waveform is drawn before a byte of audio is fetched, which is the
        // point: the shape of a voice note is most useful while deciding
        // whether to play it. Dropping it on the hydrated side once made every
        // voice note flatten to a placeholder shape the moment history was
        // reloaded — which is most of the time.
        return Some(
            MediaContent::audio(
                Arc::new(Vec::new()),
                mime,
                audio.seconds,
                audio
                    .waveform
                    .as_deref()
                    .filter(|w| !w.is_empty())
                    .map(|w| Arc::new(w.to_vec())),
            )
            .with_download(Some(downloadable)),
        );
    }

    // A document is metadata all the way down — never fetched until somebody
    // saves it, and worth a row even with nothing to fetch, because its name
    // is what the bubble says.
    if let Some(doc) = document_of(msg) {
        let mime = doc.mimetype.clone().unwrap_or_default();
        let downloadable = downloadable_of(doc, &mime, None);
        return Some(
            MediaContent::document(mime, doc.file_name.clone())
                .with_caption(doc.caption.clone())
                .with_download(downloadable),
        );
    }

    None
}

/// The media on a live message, fetching the bytes when they are worth having
/// before anybody has asked for them.
///
/// Only a sticker and an image are ever fetched here, and the order the kinds
/// are tried in is [`media_of`]'s own — whatever this fetches is what that
/// function is about to describe.
pub(super) async fn media_now(
    msg: &wa::Message,
    client: &Arc<Client>,
    eager: bool,
) -> Option<MediaContent> {
    let fetched = if let Some(sticker) = effective_sticker(msg) {
        fetch_now(client, sticker, "sticker", eager, sticker.file_length).await
    } else if let Some(image) = image_of(msg) {
        fetch_now(
            client,
            image,
            "image",
            eager && oxidezap_chat_store::business_media(msg).is_none(),
            image.file_length,
        )
        .await
    } else {
        None
    };

    let media = media_of(msg, fetched)?;
    if let Some(sticker) = effective_sticker(msg) {
        info!(
            "Sticker: mime={}, is_animated={}, is_lottie={}, size={} bytes",
            media.mime_type,
            media.is_animated,
            sticker.is_lottie.unwrap_or(false),
            media.data.len()
        );
    }
    Some(media)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_core::MediaType;
    use whatsapp_rust::buffa::MessageField;

    #[test]
    fn channel_images_keep_plaintext_download_metadata_and_fallback() {
        for static_only in [false, true] {
            let mut image = wa::message::ImageMessage::default();
            image.jpeg_thumbnail = Some(vec![1, 2]);
            image.file_sha256 = key();
            image.file_length = Some(1024);
            image.width = Some(1200);
            image.height = Some(800);
            if static_only {
                image.static_url = Some("https://mmg.whatsapp.net/channel/image".into());
            } else {
                image.direct_path = Some(DIRECT_PATH.into());
            }
            let mut message = wa::Message::default();
            message.image_message = MessageField::some(image);
            let media = media_of(&message, None).unwrap();
            assert!(media.data_is_preview);
            assert_eq!((media.width, media.height), (Some(1200), Some(800)));
            let dl = media.downloadable.unwrap();
            assert!(!dl.is_encrypted());
            assert_eq!(dl.file_sha256(), Some([7; 32].as_slice()));
            assert!(dl.is_channel_preview_candidate());
            let json = serde_json::to_string(&dl).unwrap();
            let restored: DownloadableMedia = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, dl);
            assert_eq!(restored.static_url(), dl.static_url());
            assert_eq!(
                media_of(&message, Some(vec![3, 4]))
                    .unwrap()
                    .data
                    .as_slice(),
                [3, 4]
            );
        }
    }

    #[test]
    fn a_channel_image_requires_a_complete_integrity_hash() {
        for hash in [None, Some(vec![]), Some(vec![7; 31])] {
            let mut image = wa::message::ImageMessage::default();
            image.direct_path = Some(DIRECT_PATH.into());
            image.file_sha256 = hash;
            assert!(downloadable_of(&image, "image/jpeg", None).is_none());
        }
    }

    #[test]
    fn static_media_urls_use_the_whatsapp_https_cdn() {
        assert!(is_media_cdn_url("https://mmg.whatsapp.net/image?x=1"));
        for url in [
            "http://mmg.whatsapp.net/image",
            "https://whatsapp.net.evil.test/x",
            "https://evil.test/x",
            "https://whatsapp.net@evil.test/x",
            "https://evil.test@whatsapp.net/x",
            "https://localhost/x",
        ] {
            assert!(!is_media_cdn_url(url), "{url}");
        }
    }

    /// Enough of a downloadable to build one: the three fields
    /// [`downloadable_of`] refuses without.
    const DIRECT_PATH: &str = "/v/t62.7118-24/fixture";
    fn key() -> Option<Vec<u8>> {
        Some(vec![7; 32])
    }

    fn sticker(msg: wa::message::StickerMessage) -> wa::Message {
        {
            let mut constructed: wa::Message = Default::default();
            constructed.sticker_message = MessageField::some(msg);
            constructed
        }
    }

    /// Reconnecting after a while offline hands over a batch of hundreds, and
    /// fetching a picture per message before the first bubble reaches the
    /// window spends the whole reconnection on it. The same question decides
    /// a picture nobody is going to look at soon enough to be worth the
    /// bytes.
    #[test]
    fn a_backlog_is_not_a_reason_to_fetch_every_picture() {
        // Live and small: the one case worth the round trip.
        assert!(worth_fetching_now(true, Some(64 * 1024)));
        assert!(worth_fetching_now(true, None));

        assert!(!worth_fetching_now(false, Some(64 * 1024)));
        assert!(!worth_fetching_now(false, None));
        assert!(
            !worth_fetching_now(true, Some(EAGER_MEDIA_BYTES + 1)),
            "past the ceiling the thumbnail shows and the bytes stay retryable"
        );
    }

    #[test]
    fn a_still_with_nothing_to_download_is_not_offered_as_a_preview() {
        let orphan = still_preview(vec![1, 2, 3], "image/png", "image/webp".into(), false);
        assert!(!orphan.is_preview);
        assert_eq!(orphan.mime, "image/png");

        let fetchable = still_preview(vec![1, 2, 3], "image/png", "image/webp".into(), true);
        assert!(fetchable.is_preview);

        // No bytes in hand: the mime describes the file being waited for.
        let empty = still_preview(Vec::new(), "image/png", "image/webp".into(), true);
        assert!(!empty.is_preview);
        assert_eq!(empty.mime, "image/webp");
    }

    #[test]
    fn historical_sticker_keeps_thumbnail_without_download_metadata() {
        let message = sticker({
            let mut constructed: wa::message::StickerMessage = Default::default();
            constructed.png_thumbnail = Some(vec![1, 2, 3]);
            constructed.width = Some(64);
            constructed.height = Some(64);
            constructed
        });

        let media = media_of(&message, None).expect("sticker metadata");
        assert_eq!(media.data.as_slice(), [1, 2, 3]);
        assert_eq!(media.mime_type, "image/png");
        assert!(media.downloadable.is_none());
        assert!(!media.data_is_preview);
    }

    /// Nothing to draw and nothing to fetch is not media at all — the message
    /// is a text row instead of a bubble with a hole in it.
    #[test]
    fn a_sticker_with_neither_bytes_nor_a_fetch_is_not_media() {
        assert!(media_of(&sticker(wa::message::StickerMessage::default()), None).is_none());
        assert!(
            media_of(
                &{
                    let mut constructed: wa::Message = Default::default();
                    constructed.image_message =
                        MessageField::some(wa::message::ImageMessage::default());
                    constructed
                },
                None
            )
            .is_none()
        );
    }

    /// The fetched bytes are the media, and they are not a preview of
    /// themselves: the mime is the sticker's own, not the thumbnail's.
    #[test]
    fn fetched_bytes_replace_the_still_and_keep_the_media_mime() {
        let message = sticker({
            let mut constructed: wa::message::StickerMessage = Default::default();
            constructed.png_thumbnail = Some(vec![1, 2, 3]);
            constructed.direct_path = Some(DIRECT_PATH.to_string());
            constructed.media_key = key();
            constructed.file_enc_sha256 = key();
            constructed.file_length = Some(2048);
            constructed.is_animated = Some(true);
            constructed
        });

        let still = media_of(&message, None).expect("sticker still");
        assert_eq!(still.data.as_slice(), [1, 2, 3]);
        assert_eq!(still.mime_type, "image/png");
        assert!(still.data_is_preview, "a fetch can still be made");
        assert!(still.is_animated, "what the sticker is, not what shows");

        let full = media_of(&message, Some(vec![9, 9])).expect("sticker bytes");
        assert_eq!(full.data.as_slice(), [9, 9]);
        assert_eq!(full.mime_type, "image/webp");
        assert!(!full.data_is_preview);
        assert!(full.is_animated);
    }

    #[test]
    fn an_image_carries_its_caption_and_its_fetch() {
        let message = {
            let mut constructed: wa::Message = Default::default();
            constructed.image_message = MessageField::some({
                let mut constructed: wa::message::ImageMessage = Default::default();
                constructed.jpeg_thumbnail = Some(vec![4, 5]);
                constructed.caption = Some("a photo".to_string());
                constructed.mimetype = Some("image/png".to_string());
                constructed.direct_path = Some(DIRECT_PATH.to_string());
                constructed.media_key = key();
                constructed.file_enc_sha256 = key();
                constructed.file_length = Some(4096);
                constructed.width = Some(800);
                constructed.height = Some(600);
                constructed
            });
            constructed
        };

        let still = media_of(&message, None).expect("image still");
        assert_eq!(still.media_type, MediaType::Image);
        assert_eq!(still.caption.as_deref(), Some("a photo"));
        assert_eq!(still.mime_type, "image/jpeg", "the thumbnail is in hand");
        assert!(still.data_is_preview);
        assert_eq!(still.width, Some(800));
        let downloadable = still.downloadable.as_ref().expect("image download");
        assert_eq!(downloadable.mime_type, "image/png", "what is waited for");
        assert_eq!(downloadable.file_length, 4096);

        let full = media_of(&message, Some(vec![1])).expect("image bytes");
        assert_eq!(full.mime_type, "image/png");
        assert!(!full.data_is_preview);
    }

    /// A poster frame is never the video, and the fetched bytes of the video
    /// never reach here — so both roads describe the same bubble.
    #[test]
    fn a_video_is_its_poster_frame_either_way() {
        let message = {
            let mut constructed: wa::Message = Default::default();
            constructed.video_message = MessageField::some({
                let mut constructed: wa::message::VideoMessage = Default::default();
                constructed.jpeg_thumbnail = Some(vec![2, 3]);
                constructed.seconds = Some(12);
                constructed.caption = Some("a clip".to_string());
                constructed.direct_path = Some(DIRECT_PATH.to_string());
                constructed.media_key = key();
                constructed.file_enc_sha256 = key();
                constructed
            });
            constructed
        };

        for fetched in [None, Some(vec![0xff])] {
            let media = media_of(&message, fetched).expect("video metadata");
            assert_eq!(media.media_type, MediaType::Video);
            assert_eq!(media.data.as_slice(), [2, 3]);
            assert_eq!(media.mime_type, "image/jpeg");
            assert!(media.data_is_preview);
            assert_eq!(media.duration_secs, Some(12));
            assert_eq!(media.caption.as_deref(), Some("a clip"));
            let downloadable = media.downloadable.as_ref().expect("video download");
            assert_eq!(downloadable.mime_type, "video/mp4");
            assert_eq!(downloadable.duration_secs, Some(12));
        }
    }

    /// A round video note is a `VideoMessage` in another field, and reads as
    /// an ordinary video.
    #[test]
    fn a_round_video_note_is_a_video() {
        let message = {
            let mut constructed: wa::Message = Default::default();
            constructed.ptv_message = MessageField::some({
                let mut constructed: wa::message::VideoMessage = Default::default();
                constructed.jpeg_thumbnail = Some(vec![6]);
                constructed.direct_path = Some(DIRECT_PATH.to_string());
                constructed.media_key = key();
                constructed.file_enc_sha256 = key();
                constructed
            });
            constructed
        };

        let media = media_of(&message, None).expect("ptv metadata");
        assert_eq!(media.media_type, MediaType::Video);
        assert_eq!(media.data.as_slice(), [6]);
    }

    /// The waveform is on the message, so the bars are drawn before a byte of
    /// audio is fetched — on both roads.
    #[test]
    fn a_voice_note_keeps_its_waveform_and_needs_a_fetch() {
        let with_download = {
            let mut constructed: wa::Message = Default::default();
            constructed.audio_message = MessageField::some({
                let mut constructed: wa::message::AudioMessage = Default::default();
                constructed.seconds = Some(7);
                constructed.waveform = Some(vec![0, 50, 100]);
                constructed.direct_path = Some(DIRECT_PATH.to_string());
                constructed.media_key = key();
                constructed.file_enc_sha256 = key();
                constructed
            });
            constructed
        };

        let media = media_of(&with_download, None).expect("audio metadata");
        assert_eq!(media.media_type, MediaType::Audio);
        assert!(media.data.is_empty(), "fetched when somebody presses play");
        assert_eq!(media.mime_type, "audio/ogg; codecs=opus");
        assert_eq!(media.duration_secs, Some(7));
        assert_eq!(
            media.waveform.as_deref().map(Vec::as_slice),
            Some([0, 50, 100].as_slice())
        );

        // Nothing to fetch: there is no such thing as a voice note that never
        // plays.
        let orphan = {
            let mut constructed: wa::Message = Default::default();
            constructed.audio_message = MessageField::some({
                let mut constructed: wa::message::AudioMessage = Default::default();
                constructed.seconds = Some(7);
                constructed
            });
            constructed
        };
        assert!(media_of(&orphan, None).is_none());
    }

    /// A document is worth a row for its name alone, with or without a way to
    /// fetch it.
    #[test]
    fn a_document_is_a_row_even_with_nothing_to_fetch() {
        let message = {
            let mut constructed: wa::Message = Default::default();
            constructed.document_message = MessageField::some({
                let mut constructed: wa::message::DocumentMessage = Default::default();
                constructed.file_name = Some("minutes.pdf".to_string());
                constructed.caption = Some("last week".to_string());
                constructed.mimetype = Some("application/pdf".to_string());
                constructed
            });
            constructed
        };

        let media = media_of(&message, None).expect("document metadata");
        assert_eq!(media.media_type, MediaType::Document);
        assert_eq!(media.file_name.as_deref(), Some("minutes.pdf"));
        assert_eq!(media.caption.as_deref(), Some("last week"));
        assert_eq!(media.mime_type, "application/pdf");
        assert!(media.downloadable.is_none());
        assert!(!media.data_is_preview);
    }

    /// An animated sticker can arrive wrapped in the lottie envelope, and it
    /// is the same sticker.
    #[test]
    fn a_lottie_wrapped_sticker_is_a_sticker() {
        let inner = {
            let mut constructed: wa::message::StickerMessage = Default::default();
            constructed.png_thumbnail = Some(vec![1]);
            constructed
        };
        let message = {
            let mut constructed: wa::Message = Default::default();
            constructed.lottie_sticker_message = MessageField::some({
                let mut constructed: wa::message::FutureProofMessage = Default::default();
                constructed.message = MessageField::some(sticker(inner));
                constructed
            });
            constructed
        };

        let media = media_of(&message, None).expect("lottie sticker");
        assert_eq!(media.media_type, MediaType::Sticker);
        assert_eq!(media.data.as_slice(), [1]);
    }
}

#[cfg(test)]
mod business_tests {
    use super::*;
    use whatsapp_rust::buffa::MessageField;
    #[test]
    fn interactive_header_thumbnail_is_local_and_has_no_fetch_capability() {
        let mut header = wa::message::interactive_message::Header::default();
        header.media =
            Some(wa::message::interactive_message::header::Media::JpegThumbnail(vec![1, 2, 3]));
        let mut interactive = wa::message::InteractiveMessage::default();
        interactive.header = MessageField::some(header);
        let mut message = wa::Message::default();
        message.interactive_message = MessageField::some(interactive);
        let media = media_of(&message, None).unwrap();
        assert_eq!(media.data.as_slice(), [1, 2, 3]);
        assert!(media.downloadable.is_none());
        assert!(!media.data_is_preview);
    }
}
