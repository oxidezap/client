//! Media attached to a message: what kind it is, the bytes in hand, and
//! where the full file can be fetched from.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use wacore::download::{Downloadable, MediaType as DownloadMediaType};

use super::is_false;

/// Type of media content
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaType {
    /// Image (JPEG, PNG, WebP)
    Image,
    /// Sticker (WebP, animated or static)
    Sticker,
    /// Video (thumbnail displayed, full video downloadable)
    Video,
    /// Audio (shown as placeholder)
    Audio,
    /// Document (shown as placeholder)
    Document,
}

impl MediaType {
    /// Get a display label for chat list preview
    pub fn display_label(&self) -> &'static str {
        match self {
            MediaType::Image => "📷 Photo",
            MediaType::Sticker => "🎭 Sticker",
            MediaType::Video => "🎥 Video",
            MediaType::Audio => "🎤 Voice message",
            MediaType::Document => "📄 Document",
        }
    }
}

/// Information needed to download encrypted media from WhatsApp servers.
/// This is stored separately from the thumbnail/preview data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadableMedia {
    /// Direct path for CDN URL construction
    pub direct_path: String,
    /// Encryption key for decrypting the media
    pub media_key: Vec<u8>,
    /// SHA256 of encrypted file (used for URL token)
    pub file_enc_sha256: Vec<u8>,
    /// Expected file size in bytes
    pub file_length: u64,
    /// MIME type of the actual media (e.g., "video/mp4")
    pub mime_type: String,
    /// Duration in seconds (for video/audio)
    pub duration_secs: Option<u32>,
    /// Download media type (for key derivation)
    #[serde(with = "download_type")]
    pub download_type: DownloadMediaType,
}

/// Implement Downloadable trait for DownloadableMedia to enable downloading
#[async_trait]
impl Downloadable for DownloadableMedia {
    fn direct_path(&self) -> Option<&str> {
        Some(&self.direct_path)
    }

    fn media_key(&self) -> Option<&[u8]> {
        Some(&self.media_key)
    }

    fn file_enc_sha256(&self) -> Option<&[u8]> {
        Some(&self.file_enc_sha256)
    }

    fn file_sha256(&self) -> Option<&[u8]> {
        None // Not required for download
    }

    fn file_length(&self) -> Option<u64> {
        Some(self.file_length)
    }

    fn app_info(&self) -> DownloadMediaType {
        self.download_type
    }
}

/// Media content attached to a message
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaContent {
    /// Type of media
    pub media_type: MediaType,
    /// Raw data for display (thumbnail for video, full data for images/stickers)
    ///
    /// Never serialized. For an image this is the whole photo, and a megabyte
    /// of it has no business inside a newline-delimited JSON frame; a front
    /// end in another process reads it out of the daemon's media cache under
    /// [`cache_key`](Self::cache_key) instead. Skipping it here rather than
    /// remembering not to send it is what makes that mechanical.
    ///
    /// It is also the one exception to the rule the rest of these fields keep
    /// — a field may only be skipped where its absence reads back as the
    /// value that was skipped — and what makes the exception sound is the
    /// field below: bytes are dropped from the frame only because a key names
    /// where they went. Nothing in the type ties the two together, so
    /// `media_bytes_only_leave_the_frame_once_a_key_names_them` does.
    #[serde(skip)]
    pub data: Arc<Vec<u8>>,
    /// Where the daemon's media cache holds [`data`](Self::data).
    ///
    /// Set by the daemon as it hands the message to another process, and
    /// `None` in the process that already has the bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
    /// MIME type of the display data (may differ from downloadable media)
    pub mime_type: String,
    /// Width in pixels (if known)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Height in pixels (if known)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Caption text (if any)
    #[allow(dead_code)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    /// Original file name (documents), used when saving to disk
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// Download info for fetching full media (videos, documents)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloadable: Option<DownloadableMedia>,
    /// Whether this is an animated sticker (WebP animation)
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_animated: bool,
    /// Duration in seconds (for audio/video)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    /// Whether `data` holds only a fallback thumbnail (eager download of the
    /// full media failed), so the renderer keeps offering the real download
    #[serde(default, skip_serializing_if = "is_false")]
    pub data_is_preview: bool,
    /// Amplitude envelope for a voice note, one byte per bucket in `0..=100`.
    ///
    /// WhatsApp ships this on the message itself, so the bars can be drawn
    /// before a single byte of audio is fetched — which is the point: the
    /// shape of a voice note is most useful *while deciding* whether to play
    /// it. Absent for older messages and for senders that omit it; the player
    /// falls back to a flat bar rather than inventing a shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waveform: Option<Arc<Vec<u8>>>,
}

impl MediaContent {
    /// Everything a piece of media has regardless of which kind it is: the
    /// bytes in hand and what they are.
    ///
    /// The rest of the struct defaults, and the defaults are the point — a
    /// literal spelled out at a call site re-derives fourteen answers, and it
    /// only takes one of them being wrong (a video calling its poster frame
    /// the full file, a voice note losing its waveform) for the row to render
    /// as something it is not. `cache_key` is `None` on every one of these
    /// because the daemon sets it as the message leaves the process that holds
    /// the bytes; nothing that *builds* media knows it yet.
    fn of(media_type: MediaType, data: Arc<Vec<u8>>, mime_type: String) -> Self {
        Self {
            media_type,
            data,
            cache_key: None,
            mime_type,
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

    /// A photo. `data_is_preview` says whether `data` is the photo itself or
    /// the thumbnail standing in for it until the download lands.
    pub fn image(data: Arc<Vec<u8>>, mime_type: String, data_is_preview: bool) -> Self {
        Self {
            data_is_preview,
            ..Self::of(MediaType::Image, data, mime_type)
        }
    }

    /// A sticker.
    ///
    /// `is_animated` describes the sticker *file*, not the bytes in `data`:
    /// an animated WebP is commonly carried by a still PNG preview until it is
    /// fetched, and `data_is_preview` beside it already says which of the two
    /// is in hand.
    pub fn sticker(
        data: Arc<Vec<u8>>,
        mime_type: String,
        data_is_preview: bool,
        is_animated: bool,
    ) -> Self {
        Self {
            data_is_preview,
            is_animated,
            ..Self::of(MediaType::Sticker, data, mime_type)
        }
    }

    /// A video, from its poster frame.
    ///
    /// A video's `data` is never the video: it is the JPEG poster the envelope
    /// carried, which is why the type is fixed here and the bytes count as a
    /// preview whenever there are any. Calling them the full media wrote a
    /// thumbnail under the full-video cache key, and every later read of that
    /// key handed back a still. [`adopt_full_bytes`](Self::adopt_full_bytes)
    /// is what replaces both once the file itself arrives.
    pub fn video(poster: Arc<Vec<u8>>, duration_secs: Option<u32>) -> Self {
        let data_is_preview = !poster.is_empty();
        Self {
            duration_secs,
            data_is_preview,
            ..Self::of(MediaType::Video, poster, "image/jpeg".to_string())
        }
    }

    /// A voice note or an audio file. `data` is usually empty — audio is
    /// fetched when it is played — but a note recorded here holds its own
    /// encoded bytes from the start, which is what a resend puts back on the
    /// wire.
    pub fn audio(
        data: Arc<Vec<u8>>,
        mime_type: String,
        duration_secs: Option<u32>,
        waveform: Option<Arc<Vec<u8>>>,
    ) -> Self {
        Self {
            duration_secs,
            waveform,
            ..Self::of(MediaType::Audio, data, mime_type)
        }
    }

    /// A document. Nothing is downloaded eagerly, so there are no bytes to
    /// carry — only what to call the file once there are.
    pub fn document(mime_type: String, file_name: Option<String>) -> Self {
        Self {
            file_name,
            ..Self::of(MediaType::Document, Arc::new(Vec::new()), mime_type)
        }
    }

    /// Pixel dimensions, where the envelope gave them.
    pub fn with_size(mut self, width: Option<u32>, height: Option<u32>) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// The caption sent with the media, where there was one.
    pub fn with_caption(mut self, caption: Option<String>) -> Self {
        self.caption = caption;
        self
    }

    /// Where the full file can be fetched from, where the envelope said.
    ///
    /// Takes the `Option` the download builder produces rather than a value:
    /// media with nothing to fetch is the normal case for anything composed
    /// locally, and the caller would otherwise write the same `if let` at
    /// every site.
    pub fn with_download(mut self, downloadable: Option<DownloadableMedia>) -> Self {
        self.downloadable = downloadable;
        self
    }

    /// Check if this media has inline data available
    pub fn has_data(&self) -> bool {
        !self.data.is_empty()
    }

    /// Whether [`data`](Self::data) holds a still picture — bytes any front
    /// end can decode and draw on its own.
    ///
    /// Here rather than in a front end because the answer is about the data
    /// model, not about how anything is drawn: a video carries a poster
    /// thumbnail until [`adopt_full_bytes`](Self::adopt_full_bytes) puts its
    /// own file in `data`, and from then on there is no still in there at
    /// all. A surface that asked only whether there were *bytes* handed an
    /// MP4 to an image decoder.
    pub fn has_still_image(&self) -> bool {
        // Case-insensitively: a MIME type's tokens are (RFC 2045 §5.1), and
        // `Image/JPEG` is a photo whoever sent it spelled differently.
        !self.data.is_empty()
            && self
                .mime_type
                .get(..6)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
    }

    /// Check if this media can be downloaded from server
    pub fn can_download(&self) -> bool {
        self.downloadable.is_some()
    }

    /// Check if this media can be played (has data or can be downloaded)
    pub fn can_play(&self) -> bool {
        self.has_data() || self.can_download()
    }

    /// The full media has arrived: take the bytes, and the metadata that
    /// describes *them*.
    ///
    /// A row can carry a poster frame or a thumbnail until the real file is
    /// fetched, and the fields beside it describe that stand-in — a sticker's
    /// PNG preview says `image/png` where the sticker itself is an animated
    /// WebP. Swapping only the bytes left every later reader decoding the new
    /// file under the old file's type. One place does both, because doing one
    /// without the other is the bug.
    pub fn adopt_full_bytes(&mut self, bytes: Arc<Vec<u8>>) {
        self.data = bytes;
        self.data_is_preview = false;
        if let Some(downloadable) = &self.downloadable {
            self.mime_type = downloadable.mime_type.clone();
        }
    }
}

/// `wacore`'s download [`MediaType`](DownloadMediaType) on the wire.
///
/// It is `#[non_exhaustive]` and carries no serde of its own, so this maps it
/// to a stable name rather than to a variant index that a new variant
/// upstream would silently shift. An unknown name is an error rather than a
/// guess: picking the wrong key derivation would decrypt to noise.
mod download_type {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use wacore::download::MediaType;

    fn name(value: MediaType) -> Option<&'static str> {
        Some(match value {
            MediaType::Image => "image",
            MediaType::Video => "video",
            MediaType::Audio => "audio",
            MediaType::Document => "document",
            MediaType::History => "history",
            MediaType::AppState => "app_state",
            MediaType::Sticker => "sticker",
            MediaType::StickerPack => "sticker_pack",
            MediaType::StickerPackThumbnail => "sticker_pack_thumbnail",
            MediaType::LinkThumbnail => "link_thumbnail",
            MediaType::ProductCatalogImage => "product_catalog_image",
            _ => return None,
        })
    }

    fn parse(name: &str) -> Option<MediaType> {
        Some(match name {
            "image" => MediaType::Image,
            "video" => MediaType::Video,
            "audio" => MediaType::Audio,
            "document" => MediaType::Document,
            "history" => MediaType::History,
            "app_state" => MediaType::AppState,
            "sticker" => MediaType::Sticker,
            "sticker_pack" => MediaType::StickerPack,
            "sticker_pack_thumbnail" => MediaType::StickerPackThumbnail,
            "link_thumbnail" => MediaType::LinkThumbnail,
            "product_catalog_image" => MediaType::ProductCatalogImage,
            _ => return None,
        })
    }

    pub fn serialize<S: Serializer>(value: &MediaType, s: S) -> Result<S::Ok, S::Error> {
        match name(*value) {
            Some(name) => name.serialize(s),
            None => Err(serde::ser::Error::custom(format!(
                "unnameable download media type {value:?}"
            ))),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<MediaType, D::Error> {
        let name = String::deserialize(d)?;
        parse(&name).ok_or_else(|| serde::de::Error::custom(format!("unknown media type {name}")))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Both directions from one list, so a variant added to one and not
        /// the other cannot pass unnoticed.
        #[test]
        fn every_name_round_trips() {
            for value in [
                MediaType::Image,
                MediaType::Video,
                MediaType::Audio,
                MediaType::Document,
                MediaType::History,
                MediaType::AppState,
                MediaType::Sticker,
                MediaType::StickerPack,
                MediaType::StickerPackThumbnail,
                MediaType::LinkThumbnail,
                MediaType::ProductCatalogImage,
            ] {
                let name = name(value).expect("every variant is nameable");
                assert_eq!(parse(name), Some(value), "{name}");
            }
        }

        /// An unknown name must not fall back to a variant: the wrong media
        /// type derives the wrong key and decrypts to noise.
        #[test]
        fn an_unknown_name_is_refused() {
            assert_eq!(parse("something_new"), None);
        }
    }
}

/// A photo, for the tests in this crate that need a message to carry one.
///
/// Here rather than in each test module because the timeline's merge rules are
/// largely about media that arrived as a preview and media that did not, and
/// three files were spelling the same fourteen fields out to say it.
#[cfg(test)]
pub(super) fn make_media(data: Vec<u8>, data_is_preview: bool) -> MediaContent {
    MediaContent::image(Arc::new(data), "image/jpeg".to_string(), data_is_preview)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this predicate exists for: a status posted as a photo with
    /// music arrives as an MP4, and its poster is gone the moment the file
    /// itself is fetched. Asking only whether there were bytes handed those
    /// to an image decoder.
    #[test]
    fn a_fetched_video_has_no_still_left_in_it() {
        let mut media = MediaContent::video(Arc::new(vec![1, 2, 3]), Some(15)).with_download(Some(
            DownloadableMedia {
                direct_path: "/v/t62".to_string(),
                media_key: Vec::new(),
                file_enc_sha256: Vec::new(),
                file_length: 3,
                mime_type: "video/mp4".to_string(),
                duration_secs: Some(15),
                download_type: DownloadMediaType::Video,
            },
        ));
        assert!(media.has_still_image(), "the poster is a picture");

        media.adopt_full_bytes(Arc::new(vec![4, 5, 6]));
        assert!(media.has_data(), "the video's own file is there");
        assert!(!media.has_still_image(), "and it is not a still");
    }

    /// Empty is not a picture either, whatever the type says: a row whose
    /// bytes have not arrived draws a placeholder, not a decode of nothing.
    #[test]
    fn bytes_are_half_the_question() {
        assert!(!make_media(Vec::new(), false).has_still_image());
        assert!(make_media(vec![1], false).has_still_image());
    }

    /// A MIME type's tokens are case-insensitive, and senders spell them how
    /// they like. Reading `Image/JPEG` as "not a picture" would hide a photo
    /// behind a download prompt.
    #[test]
    fn the_type_is_read_case_insensitively() {
        let mut media = make_media(vec![1], false);
        media.mime_type = "Image/JPEG".to_string();
        assert!(media.has_still_image());

        media.mime_type = "VIDEO/MP4".to_string();
        assert!(!media.has_still_image());
    }

    /// Every constructor leaves the same fields alone, and the two that are
    /// easy to get wrong by hand are the ones this checks: nothing being built
    /// knows a cache key yet, and a video's bytes are a poster rather than the
    /// file.
    #[test]
    fn a_constructor_answers_the_fields_its_caller_would_have_guessed() {
        for media in [
            MediaContent::image(Arc::new(vec![1]), "image/png".to_string(), false),
            MediaContent::sticker(Arc::new(vec![1]), "image/webp".to_string(), true, true),
            MediaContent::video(Arc::new(vec![1]), Some(15)),
            MediaContent::audio(Arc::new(Vec::new()), "audio/ogg".to_string(), Some(3), None),
            MediaContent::document("application/pdf".to_string(), Some("f.pdf".to_string())),
        ] {
            assert_eq!(media.cache_key, None, "{:?}", media.media_type);
            assert!(!media.can_download(), "{:?}", media.media_type);
        }

        let poster = MediaContent::video(Arc::new(vec![1]), Some(15));
        assert_eq!(poster.mime_type, "image/jpeg");
        assert!(poster.data_is_preview, "a poster frame is not the video");
        // No poster and no bytes: nothing is standing in for anything.
        assert!(!MediaContent::video(Arc::new(Vec::new()), None).data_is_preview);
    }

    /// These types *are* the wire format — `ipc` adds framing and nothing else
    /// — so the JSON one of them serializes to is the protocol, and a field
    /// reordered or a `skip_serializing_if` dropped while moving code is a
    /// silent break for every front end that reads the other side of it.
    /// Spelled out rather than round-tripped, which is what makes it catch the
    /// change a round trip cannot see.
    #[test]
    fn the_frame_a_media_serializes_to_is_spelled_out_here() {
        let full = MediaContent::image(Arc::new(vec![7; 4096]), "image/jpeg".to_string(), true)
            .with_size(Some(1200), Some(800))
            .with_caption(Some("uma legenda".to_string()))
            .with_download(Some(DownloadableMedia {
                direct_path: "/v/t62".to_string(),
                media_key: vec![1],
                file_enc_sha256: vec![2],
                file_length: 3,
                mime_type: "image/jpeg".to_string(),
                duration_secs: None,
                download_type: DownloadMediaType::Image,
            }));
        assert_eq!(
            serde_json::to_string(&full).unwrap(),
            r#"{"media_type":"Image","mime_type":"image/jpeg","width":1200,"height":800,"caption":"uma legenda","downloadable":{"direct_path":"/v/t62","media_key":[1],"file_enc_sha256":[2],"file_length":3,"mime_type":"image/jpeg","duration_secs":null,"download_type":"image"},"data_is_preview":true}"#
        );

        // What a frame leaves out, its reader fills in: everything defaulted
        // is absent, and the bytes never travel at all.
        assert_eq!(
            serde_json::to_string(&MediaContent::audio(
                Arc::new(vec![7; 4096]),
                "audio/ogg".to_string(),
                None,
                None,
            ))
            .unwrap(),
            r#"{"media_type":"Audio","mime_type":"audio/ogg"}"#
        );
    }
}
