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
use oxidezap_core::{DownloadableMedia, MediaContent, MediaType};
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
    match client.download(media).await {
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
    Some(DownloadableMedia {
        direct_path: media.direct_path()?.to_string(),
        media_key: media.media_key()?.to_vec(),
        file_enc_sha256: media.file_enc_sha256()?.to_vec(),
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

/// A `MediaContent` of this kind with nothing in it yet.
///
/// Every branch below fills in the four or five fields its kind actually has
/// and takes the rest from here, so a field added to the type arrives with one
/// answer for all five rather than five chances to spell it differently. There
/// is no `Default` to lean on: the kind is never a default, and `cache_key` is
/// the daemon's to set as it hands the message to another process.
fn blank(media_type: MediaType) -> MediaContent {
    MediaContent {
        media_type,
        data: Arc::new(Vec::new()),
        cache_key: None,
        mime_type: String::new(),
        width: None,
        height: None,
        caption: None,
        file_name: None,
        downloadable: None,
        is_animated: false,
        duration_secs: None,
        data_is_preview: false,
        waveform: None,
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
        return Some(MediaContent {
            data: Arc::new(still.data),
            mime_type: still.mime,
            width: sticker.width,
            height: sticker.height,
            downloadable,
            // What the sticker *is*, not what the stand-in bytes are: the
            // preview is a still, but the flag describes the file that
            // replaces it, and `data_is_preview` beside it already says which
            // of the two is in hand.
            is_animated: sticker.is_animated.unwrap_or(false),
            data_is_preview: still.is_preview,
            ..blank(MediaType::Sticker)
        });
    }

    if let Some(image) = msg.image_message.as_option() {
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
        return Some(MediaContent {
            data: Arc::new(still.data),
            mime_type: still.mime,
            width: image.width,
            height: image.height,
            caption: image.caption.clone(),
            downloadable,
            data_is_preview: still.is_preview,
            ..blank(MediaType::Image)
        });
    }

    // PTVs (round video notes) are the same proto type in a different field
    // and play like any other video.
    if let Some(video) = msg
        .ptv_message
        .as_option()
        .or(msg.video_message.as_option())
    {
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
        // poster frame, which is what the mime type beside them already says.
        // Calling them the full media wrote a thumbnail under the full-video
        // cache key, and every later read of that key handed back a still.
        return Some(MediaContent {
            data_is_preview: !thumbnail.is_empty(),
            data: Arc::new(thumbnail),
            mime_type: "image/jpeg".to_string(),
            width: video.width,
            height: video.height,
            caption: video.caption.clone(),
            downloadable,
            duration_secs: video.seconds,
            ..blank(MediaType::Video)
        });
    }

    // Audio is lazy either way: the bytes are fetched when somebody presses
    // play, so a voice note with no way to fetch them is nothing to draw.
    if let Some(audio) = msg.audio_message.as_option() {
        let mime = audio
            .mimetype
            .clone()
            .unwrap_or_else(|| "audio/ogg; codecs=opus".to_string());
        let downloadable = downloadable_of(audio, &mime, audio.seconds)?;
        return Some(MediaContent {
            mime_type: mime,
            downloadable: Some(downloadable),
            duration_secs: audio.seconds,
            // Drawn before a byte of audio is fetched, which is the point: the
            // shape of a voice note is most useful while deciding whether to
            // play it. Dropping it on the hydrated side once made every voice
            // note flatten to a placeholder shape the moment history was
            // reloaded — which is most of the time.
            waveform: audio
                .waveform
                .as_deref()
                .filter(|w| !w.is_empty())
                .map(|w| Arc::new(w.to_vec())),
            ..blank(MediaType::Audio)
        });
    }

    // A document is metadata all the way down — never fetched until somebody
    // saves it, and worth a row even with nothing to fetch, because its name
    // is what the bubble says.
    if let Some(doc) = msg.document_message.as_option() {
        let mime = doc.mimetype.clone().unwrap_or_default();
        let downloadable = downloadable_of(doc, &mime, None);
        return Some(MediaContent {
            mime_type: mime,
            caption: doc.caption.clone(),
            file_name: doc.file_name.clone(),
            downloadable,
            ..blank(MediaType::Document)
        });
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
    } else if let Some(image) = msg.image_message.as_option() {
        fetch_now(client, image, "image", eager, image.file_length).await
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
    use whatsapp_rust::buffa::MessageField;

    /// Enough of a downloadable to build one: the three fields
    /// [`downloadable_of`] refuses without.
    const DIRECT_PATH: &str = "/v/t62.7118-24/fixture";
    fn key() -> Option<Vec<u8>> {
        Some(vec![7; 32])
    }

    fn sticker(msg: wa::message::StickerMessage) -> wa::Message {
        wa::Message {
            sticker_message: MessageField::some(msg),
            ..Default::default()
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
        let message = sticker(wa::message::StickerMessage {
            png_thumbnail: Some(vec![1, 2, 3]),
            width: Some(64),
            height: Some(64),
            ..Default::default()
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
                &wa::Message {
                    image_message: MessageField::some(wa::message::ImageMessage::default()),
                    ..Default::default()
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
        let message = sticker(wa::message::StickerMessage {
            png_thumbnail: Some(vec![1, 2, 3]),
            direct_path: Some(DIRECT_PATH.to_string()),
            media_key: key(),
            file_enc_sha256: key(),
            file_length: Some(2048),
            is_animated: Some(true),
            ..Default::default()
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
        let message = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                jpeg_thumbnail: Some(vec![4, 5]),
                caption: Some("a photo".to_string()),
                mimetype: Some("image/png".to_string()),
                direct_path: Some(DIRECT_PATH.to_string()),
                media_key: key(),
                file_enc_sha256: key(),
                file_length: Some(4096),
                width: Some(800),
                height: Some(600),
                ..Default::default()
            }),
            ..Default::default()
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
        let message = wa::Message {
            video_message: MessageField::some(wa::message::VideoMessage {
                jpeg_thumbnail: Some(vec![2, 3]),
                seconds: Some(12),
                caption: Some("a clip".to_string()),
                direct_path: Some(DIRECT_PATH.to_string()),
                media_key: key(),
                file_enc_sha256: key(),
                ..Default::default()
            }),
            ..Default::default()
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
        let message = wa::Message {
            ptv_message: MessageField::some(wa::message::VideoMessage {
                jpeg_thumbnail: Some(vec![6]),
                direct_path: Some(DIRECT_PATH.to_string()),
                media_key: key(),
                file_enc_sha256: key(),
                ..Default::default()
            }),
            ..Default::default()
        };

        let media = media_of(&message, None).expect("ptv metadata");
        assert_eq!(media.media_type, MediaType::Video);
        assert_eq!(media.data.as_slice(), [6]);
    }

    /// The waveform is on the message, so the bars are drawn before a byte of
    /// audio is fetched — on both roads.
    #[test]
    fn a_voice_note_keeps_its_waveform_and_needs_a_fetch() {
        let with_download = wa::Message {
            audio_message: MessageField::some(wa::message::AudioMessage {
                seconds: Some(7),
                waveform: Some(vec![0, 50, 100]),
                direct_path: Some(DIRECT_PATH.to_string()),
                media_key: key(),
                file_enc_sha256: key(),
                ..Default::default()
            }),
            ..Default::default()
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
        let orphan = wa::Message {
            audio_message: MessageField::some(wa::message::AudioMessage {
                seconds: Some(7),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(media_of(&orphan, None).is_none());
    }

    /// A document is worth a row for its name alone, with or without a way to
    /// fetch it.
    #[test]
    fn a_document_is_a_row_even_with_nothing_to_fetch() {
        let message = wa::Message {
            document_message: MessageField::some(wa::message::DocumentMessage {
                file_name: Some("minutes.pdf".to_string()),
                caption: Some("last week".to_string()),
                mimetype: Some("application/pdf".to_string()),
                ..Default::default()
            }),
            ..Default::default()
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
        let inner = wa::message::StickerMessage {
            png_thumbnail: Some(vec![1]),
            ..Default::default()
        };
        let message = wa::Message {
            lottie_sticker_message: MessageField::some(wa::message::FutureProofMessage {
                message: MessageField::some(sticker(inner)),
            }),
            ..Default::default()
        };

        let media = media_of(&message, None).expect("lottie sticker");
        assert_eq!(media.media_type, MediaType::Sticker);
        assert_eq!(media.data.as_slice(), [1]);
    }
}
